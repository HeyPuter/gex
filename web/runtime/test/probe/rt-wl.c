/* A Wayland window for the runtime's window tests: one xdg_toplevel of a
 * solid colour, titled from argv, that says on stdout what it hears.
 *
 *   rt-wl TITLE [maximize] [move]
 *
 * "drawn W H" for each frame, "configure W H STATES" for each configure
 * (STATES: m maximized, f fullscreen, r resizing, a activated), "focus" when
 * it gets the keyboard, "key CODE" for each key pressed and "button" for
 * each press. With "move", a press asks to be moved (xdg_toplevel.move), as
 * its own title bar would; with "maximize", to be maximized. The
 * compositor's close ends it with 0.
 *
 * xdg-shell's interfaces are written out here (version 1), as
 * wayland-scanner would make them: there is no scanner in the toolchain. */
#define _GNU_SOURCE
#include <errno.h>
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
static int width = 200, height = 100, want_w, want_h, running = 1, move_on_press, maximize;
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
static void kb_enter(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *sf, struct wl_array *keys) {
  (void)d; (void)k; (void)serial; (void)sf; (void)keys;
  say("focus\n");
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
  if (caps & WL_SEAT_CAPABILITY_POINTER) wl_pointer_add_listener(wl_seat_get_pointer(s), &p_listener, NULL);
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
    else if (!strcmp(argv[i], "move")) move_on_press = 1;
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
  wl_surface_commit(surface);
  say("up\n");
  while (running && wl_display_dispatch(dpy) >= 0) {
  }
  wl_display_disconnect(dpy);
  return 0;
}
