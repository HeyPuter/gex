/* A Wayland window for the runtime's window tests: one xdg_toplevel of a
 * solid colour, titled from argv, that says on stdout what it hears.
 *
 *   rt-wl TITLE [maximize] [minimize] [move] [lock] [copy TEXT] [paste] [app ID]
 *
 * "drawn W H" for each frame, "configure W H STATES" for each configure
 * (STATES: m maximized, f fullscreen, r resizing, a activated), "focus" when
 * it gets the keyboard, "key CODE" for each key pressed and "button" for
 * each press. With "move", a press asks to be moved (xdg_toplevel.move), as
 * its own title bar would; with "maximize", to be maximized, and with
 * "minimize", minimized. "app ID" sets its app id. With "lock",
 * it asks for a locked pointer (pointer constraints, persistent) and says
 * "locked", "unlocked" and "rel DX DY" (the movement so far, in whole
 * pixels) as they come. With "copy TEXT", it sets TEXT as the selection
 * when it gets the keyboard and says "copied" (and "sent" when asked for
 * it); with "paste", it reads each selection offered to it as text and
 * says "paste TEXT". The compositor's close ends it with 0.
 *
 * xdg-shell's interfaces are written out here (version 1), as
 * wayland-scanner would make them: there is no scanner in the toolchain. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>

/* -- xdg-shell, version 1 ------------------------------------------------ */

extern const struct wl_interface xdg_wm_base_interface, xdg_positioner_interface, xdg_surface_interface,
    xdg_toplevel_interface, xdg_popup_interface;

static const struct wl_interface *types[] = {
    NULL, NULL, NULL, NULL,
    &xdg_positioner_interface,                         /* 4: create_positioner */
    &xdg_surface_interface, &wl_surface_interface,     /* 5: get_xdg_surface */
    &xdg_toplevel_interface,                           /* 7: get_toplevel */
    &xdg_popup_interface, &xdg_surface_interface, &xdg_positioner_interface, /* 8: get_popup */
    &xdg_toplevel_interface,                           /* 11: set_parent */
    &wl_seat_interface, NULL, NULL, NULL,              /* 12: show_window_menu */
    &wl_seat_interface, NULL,                          /* 16: move */
    &wl_seat_interface, NULL, NULL,                    /* 18: resize */
    &wl_output_interface,                              /* 21: set_fullscreen */
};

static const struct wl_message wm_base_requests[] = {
    {"destroy", "", types},
    {"create_positioner", "n", types + 4},
    {"get_xdg_surface", "no", types + 5},
    {"pong", "u", types},
};
static const struct wl_message wm_base_events[] = {{"ping", "u", types}};
const struct wl_interface xdg_wm_base_interface = {"xdg_wm_base", 1, 4, wm_base_requests, 1, wm_base_events};

static const struct wl_message positioner_requests[] = {
    {"destroy", "", types},
    {"set_size", "ii", types},
    {"set_anchor_rect", "iiii", types},
    {"set_anchor", "u", types},
    {"set_gravity", "u", types},
    {"set_constraint_adjustment", "u", types},
    {"set_offset", "ii", types},
};
const struct wl_interface xdg_positioner_interface = {"xdg_positioner", 1, 7, positioner_requests, 0, NULL};

static const struct wl_message surface_requests[] = {
    {"destroy", "", types},
    {"get_toplevel", "n", types + 7},
    {"get_popup", "n?oo", types + 8},
    {"set_window_geometry", "iiii", types},
    {"ack_configure", "u", types},
};
static const struct wl_message surface_events[] = {{"configure", "u", types}};
const struct wl_interface xdg_surface_interface = {"xdg_surface", 1, 5, surface_requests, 1, surface_events};

static const struct wl_message toplevel_requests[] = {
    {"destroy", "", types},
    {"set_parent", "?o", types + 11},
    {"set_title", "s", types},
    {"set_app_id", "s", types},
    {"show_window_menu", "ouii", types + 12},
    {"move", "ou", types + 16},
    {"resize", "ouu", types + 18},
    {"set_max_size", "ii", types},
    {"set_min_size", "ii", types},
    {"set_maximized", "", types},
    {"unset_maximized", "", types},
    {"set_fullscreen", "?o", types + 21},
    {"unset_fullscreen", "", types},
    {"set_minimized", "", types},
};
static const struct wl_message toplevel_events[] = {
    {"configure", "iia", types},
    {"close", "", types},
};
const struct wl_interface xdg_toplevel_interface = {"xdg_toplevel", 1, 14, toplevel_requests, 2, toplevel_events};

static const struct wl_message popup_requests[] = {{"destroy", "", types}, {"grab", "ou", types + 16}};
static const struct wl_message popup_events[] = {{"configure", "iiii", types}, {"popup_done", "", types}};
const struct wl_interface xdg_popup_interface = {"xdg_popup", 1, 2, popup_requests, 2, popup_events};

/* -- the window ------------------------------------------------------------ */

static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct wl_seat *seat;
static struct wl_proxy *wm_base;
static struct wl_surface *surface;
static struct wl_proxy *xdg_surface, *toplevel;
static int width = 200, height = 100, want_w, want_h, running = 1, move_on_press, maximize, minimize;
static const char *app_id;
static uint32_t colour = 0xff3060c0;

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...) {
  __builtin_va_list ap;
  __builtin_va_start(ap, fmt);
  vprintf(fmt, ap);
  __builtin_va_end(ap);
  fflush(stdout);
}

static void draw(void) {
  int stride = width * 4, size = stride * height;
  int fd = memfd_create("rt-wl", MFD_CLOEXEC);
  if (fd < 0 || ftruncate(fd, size) < 0) {
    perror("memfd");
    exit(1);
  }
  uint32_t *px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  for (int i = 0; i < width * height; i++) px[i] = colour;
  /* Kept mapped: gex's compositor reads the pixels where the client mapped
   * its pool, not through a mapping of its own. (Each frame's pool is left
   * mapped; the test's windows draw a handful.) */
  struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
  struct wl_buffer *buf = wl_shm_pool_create_buffer(pool, 0, width, height, stride, WL_SHM_FORMAT_XRGB8888);
  wl_shm_pool_destroy(pool);
  close(fd);
  wl_surface_attach(surface, buf, 0, 0);
  wl_surface_damage(surface, 0, 0, width, height);
  wl_surface_commit(surface);
  say("drawn %d %d\n", width, height);
}

static void ping(void *d, struct wl_proxy *base, uint32_t serial) {
  (void)d;
  wl_proxy_marshal(base, 3, serial);
}
static const void *wm_base_listener[] = {(void *)ping};

static void surface_configure(void *d, struct wl_proxy *xs, uint32_t serial) {
  (void)d;
  wl_proxy_marshal(xs, 4, serial);
  if (want_w > 0 && want_h > 0) {
    width = want_w;
    height = want_h;
  }
  draw();
}
static const void *surface_listener[] = {(void *)surface_configure};

static void toplevel_configure(void *d, struct wl_proxy *t, int32_t w, int32_t h, struct wl_array *states) {
  (void)d;
  (void)t;
  char s[8] = "";
  uint32_t *st;
  wl_array_for_each(st, states) {
    const char *c = *st == 1 ? "m" : *st == 2 ? "f" : *st == 3 ? "r" : *st == 4 ? "a" : "";
    if (strlen(s) < sizeof s - 1) strcat(s, c);
  }
  say("configure %d %d %s\n", w, h, *s ? s : "-");
  want_w = w;
  want_h = h;
}
static void toplevel_close(void *d, struct wl_proxy *t) {
  (void)d;
  (void)t;
  say("close\n");
  running = 0;
}
static const void *toplevel_listener[] = {(void *)toplevel_configure, (void *)toplevel_close};

static void kb_keymap(void *d, struct wl_keyboard *k, uint32_t f, int32_t fd, uint32_t s) {
  (void)d; (void)k; (void)f; (void)s;
  close(fd);
}
/* -- the selection -------------------------------------------------------- */

static struct wl_data_device_manager *ddm;
static struct wl_data_device *data_device;
static const char *copy_text;
static int want_paste, paste_fd = -1, copied;
static char paste_buf[65536];
static size_t paste_len;

static void src_target(void *d, struct wl_data_source *s, const char *m) {
  (void)d; (void)s; (void)m;
}
static void src_send(void *d, struct wl_data_source *s, const char *mime, int32_t fd) {
  (void)d; (void)s; (void)mime;
  if (write(fd, copy_text, strlen(copy_text)) < 0) perror("write");
  close(fd);
  say("sent\n");
}
static void src_cancelled(void *d, struct wl_data_source *s) {
  (void)d;
  wl_data_source_destroy(s);
}
static const struct wl_data_source_listener src_listener = {src_target, src_send, src_cancelled, NULL, NULL, NULL};

static void offer_offer(void *d, struct wl_data_offer *o, const char *m) {
  (void)d; (void)o; (void)m;
}
static const struct wl_data_offer_listener offer_listener = {offer_offer, NULL, NULL};
static void dd_offer(void *d, struct wl_data_device *dev, struct wl_data_offer *o) {
  (void)d; (void)dev;
  wl_data_offer_add_listener(o, &offer_listener, NULL);
}
static void dd_selection(void *d, struct wl_data_device *dev, struct wl_data_offer *o) {
  (void)d; (void)dev;
  if (!o || !want_paste) return;
  int p[2];
  if (pipe2(p, O_CLOEXEC) < 0) return;
  wl_data_offer_receive(o, "text/plain;charset=utf-8", p[1]);
  close(p[1]);
  if (paste_fd >= 0) close(paste_fd);
  paste_fd = p[0];
  paste_len = 0;
}
static void dd_enter(void *d, struct wl_data_device *dev, uint32_t s, struct wl_surface *sf, wl_fixed_t x, wl_fixed_t y,
                     struct wl_data_offer *o) {
  (void)d; (void)dev; (void)s; (void)sf; (void)x; (void)y; (void)o;
}
static void dd_leave(void *d, struct wl_data_device *dev) {
  (void)d; (void)dev;
}
static void dd_motion(void *d, struct wl_data_device *dev, uint32_t t, wl_fixed_t x, wl_fixed_t y) {
  (void)d; (void)dev; (void)t; (void)x; (void)y;
}
static void dd_drop(void *d, struct wl_data_device *dev) {
  (void)d; (void)dev;
}
static const struct wl_data_device_listener dd_listener = {dd_offer, dd_enter, dd_leave, dd_motion, dd_drop, dd_selection};

static void kb_enter(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *sf, struct wl_array *keys) {
  (void)d; (void)k; (void)sf; (void)keys;
  say("focus\n");
  if (copy_text && data_device && !copied) {
    struct wl_data_source *src = wl_data_device_manager_create_data_source(ddm);
    wl_data_source_add_listener(src, &src_listener, NULL);
    wl_data_source_offer(src, "text/plain;charset=utf-8");
    wl_data_device_set_selection(data_device, src, serial);
    copied = 1;
    say("copied\n");
  }
}
static void kb_leave(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *sf) {
  (void)d; (void)k; (void)serial; (void)sf;
}
static void kb_key(void *d, struct wl_keyboard *k, uint32_t serial, uint32_t time, uint32_t key, uint32_t state) {
  (void)d; (void)k; (void)serial; (void)time;
  if (state) say("key %u\n", key);
}
static void kb_mods(void *d, struct wl_keyboard *k, uint32_t s, uint32_t a, uint32_t b, uint32_t c, uint32_t g) {
  (void)d; (void)k; (void)s; (void)a; (void)b; (void)c; (void)g;
}
static void kb_repeat(void *d, struct wl_keyboard *k, int32_t r, int32_t de) {
  (void)d; (void)k; (void)r; (void)de;
}
/* -- pointer constraints and relative pointer, version 1 ----------------- */

extern const struct wl_interface zwp_relative_pointer_manager_v1_interface, zwp_relative_pointer_v1_interface,
    zwp_pointer_constraints_v1_interface, zwp_locked_pointer_v1_interface;
static const struct wl_interface *ctypes[] = {
    NULL, NULL, NULL, NULL, NULL, NULL,
    &zwp_relative_pointer_v1_interface, &wl_pointer_interface,                  /* 6: get_relative_pointer */
    &zwp_locked_pointer_v1_interface, &wl_surface_interface, &wl_pointer_interface, &wl_region_interface, NULL,
    /* 8: lock_pointer */
};
static const struct wl_message rpm_requests[] = {{"destroy", "", ctypes}, {"get_relative_pointer", "no", ctypes + 6}};
const struct wl_interface zwp_relative_pointer_manager_v1_interface = {
    "zwp_relative_pointer_manager_v1", 1, 2, rpm_requests, 0, NULL};
static const struct wl_message rp_requests[] = {{"destroy", "", ctypes}};
static const struct wl_message rp_events[] = {{"relative_motion", "uuffff", ctypes}};
const struct wl_interface zwp_relative_pointer_v1_interface = {"zwp_relative_pointer_v1", 1, 1, rp_requests, 1, rp_events};
static const struct wl_message pc_requests[] = {
    {"destroy", "", ctypes}, {"lock_pointer", "noo?ou", ctypes + 8}, {"confine_pointer", "noo?ou", ctypes + 8}};
const struct wl_interface zwp_pointer_constraints_v1_interface = {"zwp_pointer_constraints_v1", 1, 3, pc_requests, 0, NULL};
static const struct wl_message lp_requests[] = {
    {"destroy", "", ctypes}, {"set_cursor_position_hint", "ff", ctypes}, {"set_region", "?o", ctypes + 11}};
static const struct wl_message lp_events[] = {{"locked", "", ctypes}, {"unlocked", "", ctypes}};
const struct wl_interface zwp_locked_pointer_v1_interface = {"zwp_locked_pointer_v1", 1, 3, lp_requests, 2, lp_events};

static void *rel_manager, *constraints;
static struct wl_pointer *pointer;
static int want_lock;
static double rel_x, rel_y;

static void lp_locked(void *d, void *l) {
  (void)d; (void)l;
  say("locked\n");
}
static void lp_unlocked(void *d, void *l) {
  (void)d; (void)l;
  say("unlocked\n");
}
static void (*lp_listener[])(void) = {(void (*)(void))lp_locked, (void (*)(void))lp_unlocked};
static void rp_motion(void *d, void *p, uint32_t hi, uint32_t lo, wl_fixed_t dx, wl_fixed_t dy, wl_fixed_t ux, wl_fixed_t uy) {
  (void)d; (void)p; (void)hi; (void)lo; (void)ux; (void)uy;
  rel_x += wl_fixed_to_double(dx);
  rel_y += wl_fixed_to_double(dy);
  say("rel %d %d\n", (int)rel_x, (int)rel_y);
}
static void (*rp_listener[])(void) = {(void (*)(void))rp_motion};

static const struct wl_keyboard_listener kb_listener = {kb_keymap, kb_enter, kb_leave, kb_key, kb_mods, kb_repeat};

static void p_enter(void *d, struct wl_pointer *p, uint32_t s, struct wl_surface *sf, wl_fixed_t x, wl_fixed_t y) {
  (void)d; (void)p; (void)s; (void)sf; (void)x; (void)y;
}
static void p_leave(void *d, struct wl_pointer *p, uint32_t s, struct wl_surface *sf) {
  (void)d; (void)p; (void)s; (void)sf;
}
static void p_motion(void *d, struct wl_pointer *p, uint32_t t, wl_fixed_t x, wl_fixed_t y) {
  (void)d; (void)p; (void)t; (void)x; (void)y;
}
static void p_button(void *d, struct wl_pointer *p, uint32_t serial, uint32_t t, uint32_t b, uint32_t state) {
  (void)d; (void)p; (void)t; (void)b;
  if (!state) return;
  say("button\n");
  if (move_on_press) wl_proxy_marshal(toplevel, 5, seat, serial);
  if (maximize) wl_proxy_marshal(toplevel, 9);
  if (minimize) wl_proxy_marshal(toplevel, 13);
}
static void p_axis(void *d, struct wl_pointer *p, uint32_t t, uint32_t a, wl_fixed_t v) {
  (void)d; (void)p; (void)t; (void)a; (void)v;
}
static void p_frame(void *d, struct wl_pointer *p) {
  (void)d; (void)p;
}
static void p_source(void *d, struct wl_pointer *p, uint32_t s) {
  (void)d; (void)p; (void)s;
}
static void p_stop(void *d, struct wl_pointer *p, uint32_t t, uint32_t a) {
  (void)d; (void)p; (void)t; (void)a;
}
static void p_discrete(void *d, struct wl_pointer *p, uint32_t a, int32_t v) {
  (void)d; (void)p; (void)a; (void)v;
}
static const struct wl_pointer_listener p_listener = {p_enter, p_leave, p_motion, p_button, p_axis, p_frame, p_source, p_stop, p_discrete, p_discrete, NULL};

static void seat_caps(void *d, struct wl_seat *s, uint32_t caps) {
  (void)d;
  static int done;
  if (done) return;
  done = 1;
  if (caps & WL_SEAT_CAPABILITY_KEYBOARD) wl_keyboard_add_listener(wl_seat_get_keyboard(s), &kb_listener, NULL);
  if (caps & WL_SEAT_CAPABILITY_POINTER) {
    pointer = wl_seat_get_pointer(s);
    wl_pointer_add_listener(pointer, &p_listener, NULL);
  }
}
static void seat_name(void *d, struct wl_seat *s, const char *n) {
  (void)d; (void)s; (void)n;
}
static const struct wl_seat_listener seat_listener = {seat_caps, seat_name};

static void global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v) {
  (void)d;
  if (!strcmp(iface, "wl_compositor")) compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
  else if (!strcmp(iface, "wl_shm")) shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
  else if (!strcmp(iface, "wl_seat")) {
    seat = wl_registry_bind(r, name, &wl_seat_interface, v < 5 ? v : 5);
    wl_seat_add_listener(seat, &seat_listener, NULL);
  } else if (!strcmp(iface, "xdg_wm_base")) {
    wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
    wl_proxy_add_listener(wm_base, (void (**)(void))wm_base_listener, NULL);
  } else if (!strcmp(iface, "wl_data_device_manager")) {
    ddm = wl_registry_bind(r, name, &wl_data_device_manager_interface, 3);
  } else if (!strcmp(iface, "zwp_relative_pointer_manager_v1")) {
    rel_manager = wl_registry_bind(r, name, &zwp_relative_pointer_manager_v1_interface, 1);
  } else if (!strcmp(iface, "zwp_pointer_constraints_v1")) {
    constraints = wl_registry_bind(r, name, &zwp_pointer_constraints_v1_interface, 1);
  }
}
static void global_remove(void *d, struct wl_registry *r, uint32_t name) {
  (void)d; (void)r; (void)name;
}
static const struct wl_registry_listener registry_listener = {global, global_remove};

int main(int argc, char **argv) {
  const char *title = argc > 1 ? argv[1] : "rt-wl";
  for (int i = 2; i < argc; i++) {
    if (!strcmp(argv[i], "maximize")) maximize = 1;
    else if (!strcmp(argv[i], "minimize")) minimize = 1;
    else if (!strcmp(argv[i], "app") && i + 1 < argc) app_id = argv[++i];
    else if (!strcmp(argv[i], "move")) move_on_press = 1;
    else if (!strcmp(argv[i], "lock")) want_lock = 1;
    else if (!strcmp(argv[i], "paste")) want_paste = 1;
    else if (!strcmp(argv[i], "copy") && i + 1 < argc) copy_text = argv[++i];
  }
  struct wl_display *dpy = wl_display_connect(NULL);
  if (!dpy) {
    fprintf(stderr, "rt-wl: no Wayland display: %s\n", strerror(errno));
    return 1;
  }
  wl_registry_add_listener(wl_display_get_registry(dpy), &registry_listener, NULL);
  wl_display_roundtrip(dpy);
  if (!compositor || !shm || !wm_base) {
    fprintf(stderr, "rt-wl: the compositor lacks wl_compositor, wl_shm or xdg_wm_base\n");
    return 1;
  }
  surface = wl_compositor_create_surface(compositor);
  xdg_surface = wl_proxy_marshal_constructor(wm_base, 2, &xdg_surface_interface, NULL, surface);
  wl_proxy_add_listener(xdg_surface, (void (**)(void))surface_listener, NULL);
  toplevel = wl_proxy_marshal_constructor(xdg_surface, 1, &xdg_toplevel_interface, NULL);
  wl_proxy_add_listener(toplevel, (void (**)(void))toplevel_listener, NULL);
  wl_proxy_marshal(toplevel, 2, title);
  if (app_id) wl_proxy_marshal(toplevel, 3, app_id);
  wl_surface_commit(surface);
  if (want_lock) {
    wl_display_roundtrip(dpy);
    if (!rel_manager || !constraints || !pointer) {
      fprintf(stderr, "rt-wl: no pointer constraints or relative pointer\n");
      return 1;
    }
    void *rp = wl_proxy_marshal_constructor(rel_manager, 1, &zwp_relative_pointer_v1_interface, NULL, pointer);
    wl_proxy_add_listener(rp, rp_listener, NULL);
    void *lp = wl_proxy_marshal_constructor(constraints, 1, &zwp_locked_pointer_v1_interface, NULL, surface, pointer, NULL, 2);
    wl_proxy_add_listener(lp, lp_listener, NULL);
    wl_surface_commit(surface);
  }
  if ((copy_text || want_paste) && ddm && seat) {
    data_device = wl_data_device_manager_get_data_device(ddm, seat);
    wl_data_device_add_listener(data_device, &dd_listener, NULL);
  }
  say("up\n");
  while (running) {
    while (wl_display_prepare_read(dpy) != 0) wl_display_dispatch_pending(dpy);
    wl_display_flush(dpy);
    struct pollfd p[2] = {{wl_display_get_fd(dpy), POLLIN, 0}, {paste_fd, POLLIN, 0}};
    if (poll(p, paste_fd >= 0 ? 2 : 1, -1) < 0) {
      wl_display_cancel_read(dpy);
      if (errno == EINTR) continue;
      break;
    }
    if (p[0].revents & (POLLIN | POLLHUP | POLLERR)) {
      if (wl_display_read_events(dpy) < 0) break;
    } else {
      wl_display_cancel_read(dpy);
    }
    if (wl_display_dispatch_pending(dpy) < 0) break;
    if (paste_fd >= 0 && (p[1].revents & (POLLIN | POLLHUP))) {
      ssize_t n = read(paste_fd, paste_buf + paste_len, sizeof paste_buf - 1 - paste_len);
      if (n > 0) {
        paste_len += (size_t)n;
      } else if (n == 0 || errno != EAGAIN) {
        paste_buf[paste_len] = 0;
        say("paste %s\n", paste_buf);
        close(paste_fd);
        paste_fd = -1;
      }
    }
  }
  wl_display_disconnect(dpy);
  return 0;
}
