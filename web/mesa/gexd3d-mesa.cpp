// SPDX-License-Identifier: AGPL-3.0-only
/*
 * The guest's D3D device, in the same module as its GL device (gexgl-mesa.cpp).
 *
 * A D3D program's calls reach here as a stream of records (dxvk's
 * src/dirk/wire; Mesa pins dxvk, subprojects/dxvk.wrap) through the same
 * ring, the same ioctls and the same client token as GL: the ring's magic
 * says which decoder runs it. The records are called on DXVK's D3D11 and
 * DXGI front end, which draws on a gex_wgpu screen of its own on the page's
 * WebGPU device.
 *
 * A frame is shown the way a GL client's is: a present files it under the
 * client's window in gexgl-mesa.cpp's surface table, and the page's display
 * and the compositor take it from there.
 */
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <ctime>
#include <map>
#include <utility>
#include <vector>

#include <emscripten/emscripten.h>
#include <webgpu/webgpu.h>

#include "pipe/p_defines.h"
#include "pipe/p_state.h"
#include "util/format/u_formats.h"
#include "gex_wgpu/gex_wgpu_public.h"

#include "dirk/dirk.h"
#include "dirk_wire_host.h"

extern "C" WGPUDevice emscripten_webgpu_get_device(void);
/* gexgl-mesa.cpp: a frame for one of a client's windows. */
extern "C" uint64_t gex_wgpu_presents;
extern "C" int64_t gex_wgpu_time_replay;
extern "C" void gex_gl_external_frame(uint32_t client, uint32_t handle, unsigned kind,
                                      unsigned id, unsigned token,
                                      struct pipe_resource *image, unsigned width,
                                      unsigned height);

/* DIRK_IMPORT_PROBE=1 (`?mesaenv=`, off by default): what a call out to
 * JavaScript costs on this thread, against a plain wasm call and a clock
 * read, inside the JSPI-wrapped submit (every 4096 submits) and outside it,
 * from the page's display call (every 1024). A page whose Mesa imports are
 * counted (globalThis.gexImportReport) is asked for its counts then too. */
EM_JS(void, gex_d3d_noop_import, (), {});
EM_JS(void, gex_d3d_import_report, (double presents), {
    if (globalThis.gexImportReport) globalThis.gexImportReport(presents);
});

namespace {

int probe_wasm_inner(int x) { return x * 3 + 1; }
int (*volatile probe_wasm)(int) = probe_wasm_inner;

bool probe_on() {
    static int on = -1;
    if (on < 0) {
        const char *v = getenv("DIRK_IMPORT_PROBE");
        on = v && *v && *v != '0';
    }
    return on != 0;
}

void import_probe(const char *where) {
    const int n = 4096;
    const double t0 = emscripten_get_now();
    for (int i = 0; i < n; ++i) gex_d3d_noop_import();
    const double t1 = emscripten_get_now();
    struct timespec ts;
    int64_t sink = 0;
    for (int i = 0; i < n; ++i) {
        clock_gettime(CLOCK_MONOTONIC, &ts);
        sink += ts.tv_nsec;
    }
    const double t2 = emscripten_get_now();
    for (int i = 0; i < n; ++i) sink += probe_wasm((int)sink);
    const double t3 = emscripten_get_now();
    double now = 0;
    for (int i = 0; i < n; ++i) now += emscripten_get_now();
    const double t4 = emscripten_get_now();
    fprintf(stderr,
            "gex-d3d: import probe (%s): a no-op JS import %.3f us, a clock read %.3f us "
            "(emscripten_get_now %.3f us), a wasm call %.4f us%s\n",
            where, (t1 - t0) * 1e3 / n, (t2 - t1) * 1e3 / n, (t4 - t3) * 1e3 / n,
            (t3 - t2) * 1e3 / n, sink == 1 || now == 1 ? " " : "");
}

/* A window a client makes swap chains for: the surface that shows it. */
struct window {
    unsigned kind = 0, id = 0, width = 0, height = 0;
    /* The drawable handle its frames are filed under in the GL surface
     * table: above the handles a client's EGL picks. */
    uint32_t handle = 0;
};

struct client {
    dirk_wire_client *wire = nullptr;
    std::map<uint64_t, window> windows;
    uint32_t next_handle = 0x80000000u;
};

std::map<uint32_t, client> clients;
/* The client whose records are being read: what the host callbacks below,
 * called from inside a submit, are about. */
uint32_t current = 0;
std::vector<uint8_t> reply;
bool started = false;

/* DIRK_WIRE_STATS=1 (`?mesaenv=`, off by default): the transport's time per
 * submit, every 64 frames presented. The page's side and the bridge's are
 * told (gex_d3d_note_js, gex_d3d_note_bridge); decoding is timed here. */
struct transport_stats {
    uint64_t frames = 0, submits[2] = {0, 0}, bridged = 0, bytes = 0;
    double copy_ms = 0, js_ms[2] = {0, 0}, decode_ms = 0, wake_ms = 0;
    double claim_us = 0, channel_us = 0, proxy_us = 0;
};
transport_stats tx, tx_was;

bool stats_on() {
    static int on = -1;
    if (on < 0) {
        const char *v = getenv("DIRK_WIRE_STATS");
        on = v && *v && *v != '0';
    }
    return on != 0;
}

void stats_frame() {
    if (!stats_on() || ++tx.frames % 64) return;
    const double n = double(tx.frames - tx_was.frames);
    const uint64_t un = tx.submits[0] - tx_was.submits[0], wa = tx.submits[1] - tx_was.submits[1];
    const uint64_t all = un + wa, br = tx.bridged - tx_was.bridged;
    auto per = [](double v, uint64_t k) { return k ? v / double(k) : 0.0; };
    fprintf(stderr,
            "gex-d3d: stats, frame %llu: per frame %.1f submits unwaited, %.1f waited, %.1f KiB; per submit: "
            "page wake %.3f ms, JS copy %.3f ms, decode %.3f ms (%.3f in JS unwaited, %.3f waited); "
            "client side: claim %.3f ms, channel copy %.3f ms, all of submit %.3f ms\n",
            (unsigned long long)tx.frames, double(un) / n, double(wa) / n,
            double(tx.bytes - tx_was.bytes) / n / 1024, per(tx.wake_ms - tx_was.wake_ms, all),
            per(tx.copy_ms - tx_was.copy_ms, all), per(tx.decode_ms - tx_was.decode_ms, all),
            per(tx.js_ms[0] - tx_was.js_ms[0], un), per(tx.js_ms[1] - tx_was.js_ms[1], wa),
            per(tx.claim_us - tx_was.claim_us, br) / 1000, per(tx.channel_us - tx_was.channel_us, br) / 1000,
            per(tx.proxy_us - tx_was.proxy_us, br) / 1000);
    tx_was = tx;
}

window *window_of(void *hwnd) {
    auto c = clients.find(current);
    if (c == clients.end()) return nullptr;
    /* The window as DXVK has it: the guest's HWND, cut to a pointer. */
    auto w = c->second.windows.find((uint64_t)(uintptr_t)hwnd);
    if (w != c->second.windows.end()) return &w->second;
    for (auto &e : c->second.windows)
        if ((uint32_t)e.first == (uint32_t)(uintptr_t)hwnd) return &e.second;
    return nullptr;
}

bool host_window_size(void *hwnd, uint32_t *width, uint32_t *height) {
    window *w = window_of(hwnd);
    if (!w) return false;
    *width = w->width;
    *height = w->height;
    return true;
}

void host_display_mode(uint32_t *width, uint32_t *height, uint32_t *refresh_mhz) {
    *width = 1920;
    *height = 1080;
    *refresh_mhz = 60000;
}

/* What the canvases take: frames are copied texture to texture. */
uint32_t host_surface_format(void *) {
    return PIPE_FORMAT_R8G8B8A8_UNORM;
}

void host_present(void *hwnd, struct pipe_resource *image, uint32_t width, uint32_t height,
                  uint32_t) {
    window *w = window_of(hwnd);
    if (!w) {
        static bool said;
        if (!said) {
            said = true;
            fprintf(stderr, "gex-d3d: a present to a window no surface was named for\n");
        }
        return;
    }
    stats_frame();
    /* The driver's frame count (its GEX_WGPU_TIMING report is per frame):
     * a D3D present does not pass through the window-system present that
     * counts GL's. */
    ++gex_wgpu_presents;
    /* Every 600 presents, as the GL decoder reports its frames: the count a
     * frame rate is measured from. Not the compositor's frames, which also
     * show any GL the same process draws. */
    static uint64_t presents;
    if (presents++ % 600 == 0)
        fprintf(stderr, "gex-d3d: present %llu\n", (unsigned long long)presents);
    /* A window with no surface (id 0): a program drawing off screen, whose
     * frames go nowhere. */
    if (!w->id) return;
    gex_gl_external_frame(current, w->handle, w->kind, w->id, current, image, width, height);
}

const dirk_host host = {
    host_window_size,
    host_display_mode,
    host_surface_format,
    host_present,
};

void on_surface(void *user, uint64_t hwnd, uint32_t kind, uint32_t id, uint32_t width,
                uint32_t height) {
    client &c = *static_cast<client *>(user);
    window &w = c.windows[hwnd];
    if (!w.handle) w.handle = c.next_handle++;
    w.kind = kind;
    w.id = id;
    w.width = width;
    w.height = height;
}

bool start() {
    if (started) return true;
    WGPUDevice device = emscripten_webgpu_get_device();
    struct pipe_screen *screen = device ? gex_wgpu_screen_create(device) : nullptr;
    if (!screen) return false;
    /* A program may make several devices, each a context on this screen,
     * and all are driven from this thread: dirk runs DXVK's command stream
     * inline, and a client's records are read one submit at a time. */
    gex_wgpu_screen_one_thread(screen);
    dirk_init(screen, &host);
    started = true;
    return true;
}

} // namespace

extern "C" {

/* Run a D3D client's records. 0, or negative when they are refused: the
 * client's stream is malformed (-2) or there is no device (-1). The answer to
 * a last record that waits is gex_d3d_reply_data/len. */
EMSCRIPTEN_KEEPALIVE int gex_d3d_submit(const uint8_t *data, uint32_t len, uint32_t client_id) {
    reply.clear();
    if (!start()) return -1;
    client &c = clients[client_id];
    if (!c.wire) {
        dirk_wire_host_ops ops = { on_surface, &c };
        c.wire = dirk_wire_client_create(&ops);
        if (!c.wire) return -1;
    }
    static uint64_t submits;
    if (probe_on() && ++submits % 4096 == 0) import_probe("inside the JSPI submit");
    const uint32_t was = current;
    current = client_id;
    const uint8_t *answer = nullptr;
    size_t answer_len = 0;
    const double t0 = emscripten_get_now();
    const bool ok = dirk_wire_client_submit(c.wire, data, len, &answer, &answer_len);
    const double t1 = emscripten_get_now();
    /* For the driver's timing report, which splits it by what the driver
     * spent of it. Two clock reads a submit, a few submits a frame. */
    gex_wgpu_time_replay += (int64_t)((t1 - t0) * 1e6);
    if (stats_on()) {
        tx.decode_ms += t1 - t0;
        tx.bytes += len;
    }
    current = was;
    if (!ok) {
        static bool said;
        if (!said) {
            said = true;
            fprintf(stderr, "gex-d3d: client %u: a malformed stream, refused\n", client_id);
        }
        return -2;
    }
    if (answer_len) reply.assign(answer, answer + answer_len);
    return 0;
}

/* For the page (mesa-gl.js, gl-bridge.mjs): whether to time the transport,
 * and what it timed. */
EMSCRIPTEN_KEEPALIVE int gex_d3d_stats_on() { return stats_on(); }
EMSCRIPTEN_KEEPALIVE void gex_d3d_note_js(double copy_ms, double js_ms, int waited) {
    tx.copy_ms += copy_ms;
    tx.js_ms[waited != 0] += js_ms;
    tx.submits[waited != 0]++;
}
EMSCRIPTEN_KEEPALIVE void gex_d3d_note_bridge(double wake_ms, double claim_us, double channel_us,
                                              double proxy_us, double n) {
    tx.wake_ms += wake_ms;
    tx.claim_us += claim_us;
    tx.channel_us += channel_us;
    tx.proxy_us += proxy_us;
    tx.bridged += uint64_t(n);
}

/* gexgl-mesa.cpp's gex_gl_display, which the page calls directly, off the
 * JSPI stack. */
void gex_d3d_display_tick() {
    static uint64_t n;
    if (!probe_on() || ++n % 1024) return;
    import_probe("outside JSPI, same worker");
    gex_d3d_import_report((double)gex_wgpu_presents);
}

EMSCRIPTEN_KEEPALIVE uint32_t gex_d3d_reply_len() { return reply.size(); }
EMSCRIPTEN_KEEPALIVE const uint8_t *gex_d3d_reply_data() { return reply.data(); }

/* The process is gone: every object it had goes, as its exit would have
 * released them. Its windows go with gex_gl_forget_client. */
EMSCRIPTEN_KEEPALIVE void gex_d3d_forget_client(uint32_t client_id) {
    auto c = clients.find(client_id);
    if (c == clients.end()) return;
    const uint32_t was = current;
    current = client_id;
    if (c->second.wire) dirk_wire_client_destroy(c->second.wire);
    current = was;
    clients.erase(c);
}

}
