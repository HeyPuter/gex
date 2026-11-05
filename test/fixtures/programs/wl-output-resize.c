/* A connected client hears the output's resize (web/test/check-output-resize.mjs).
 * A raw Wayland client (no libwayland): it binds wl_output, and wl_seat with a
 * pointer (a client that takes input, as Wine's does), then waits in poll for
 * events and prints each size its output settles on (`mode` then `done`):
 *
 *   bound 1300x713       the size at the bind
 *   size 1000x650        a resize while connected
 *
 * When the compositor has zxdg_output_manager_v1 it also describes the output
 * with a zxdg_output_v1 (version 3, which `wl_output.done` ends) and prints its
 * logical size each time that settles:
 *
 *   logical 1000x650
 *
 * Exits 0 once it has printed the size given as its first argument (WxH), as
 * both the mode and (with the manager) the logical size, or 1 after the
 * seconds given as its second (default 20). */
#define _GNU_SOURCE
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static int sock;
static uint32_t buf[64];
static int len, at;

static void die(const char *why) {
	printf("wl-output-resize: %s\n", why);
	exit(1);
}

static void begin(uint32_t obj, uint32_t op) {
	at = len;
	buf[len++] = obj;
	buf[len++] = op;
}
static void put(uint32_t w) { buf[len++] = w; }
static void put_str(const char *s) {
	uint32_t n = strlen(s) + 1;
	put(n);
	memset(&buf[len], 0, (n + 3) & ~3u);
	memcpy(&buf[len], s, n);
	len += (n + 3) / 4;
}
static void end(void) { buf[at + 1] |= (uint32_t)(len - at) * 4 << 16; }
static void flush(void) {
	if (write(sock, buf, len * 4) != len * 4) die("write failed");
	len = 0;
}

static double now_s(void) {
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec + t.tv_nsec / 1e9;
}

enum { OUTPUT = 4, SEAT = 5, POINTER = 6, SYNC = 7, XDG_MANAGER = 8, XDG_OUTPUT = 9 };
static uint8_t in[65536];
static size_t inlen;
static uint32_t output_name, seat_name, xdg_name, synced;
static int mode_w, mode_h, bound, shown_w, shown_h;
static int log_w, log_h, log_shown_w, log_shown_h;

/* Take every whole event in `in`. */
static void events(void) {
	size_t pos = 0;
	while (inlen - pos >= 8) {
		uint32_t h[2];
		memcpy(h, in + pos, 8);
		uint32_t obj = h[0], op = h[1] & 0xffff, size = h[1] >> 16;
		if (size < 8) die("a bad event");
		if (inlen - pos < size) break;
		uint32_t a[16] = { 0 };
		memcpy(a, in + pos + 8, size - 8 < sizeof a ? size - 8 : sizeof a);
		if (obj == 1 && op == 0) die("wl_display.error");
		if (obj == 2 && op == 0) { /* wl_registry.global(name, interface, version) */
			const char *iface = (const char *)(in + pos + 16);
			if (!strcmp(iface, "wl_output")) output_name = a[0];
			if (!strcmp(iface, "wl_seat")) seat_name = a[0];
			if (!strcmp(iface, "zxdg_output_manager_v1")) xdg_name = a[0];
		}
		if (obj == SYNC && op == 0) synced = 1;
		if (obj == OUTPUT && op == 1) { /* mode(flags, width, height, refresh) */
			mode_w = (int)a[1];
			mode_h = (int)a[2];
		}
		if (obj == XDG_OUTPUT && op == 1) { /* logical_size(width, height) */
			log_w = (int)a[0];
			log_h = (int)a[1];
		}
		if (obj == XDG_OUTPUT && op == 2) die("zxdg_output_v1.done at version 3");
		if (obj == OUTPUT && op == 2 && (mode_w != shown_w || mode_h != shown_h)) { /* done */
			printf("%s %dx%d\n", bound ? "size" : "bound", mode_w, mode_h);
			bound = 1;
			shown_w = mode_w;
			shown_h = mode_h;
		}
		if (obj == OUTPUT && op == 2 && (log_w != log_shown_w || log_h != log_shown_h)) {
			printf("logical %dx%d\n", log_w, log_h);
			log_shown_w = log_w;
			log_shown_h = log_h;
		}
		pos += size;
	}
	memmove(in, in + pos, inlen - pos);
	inlen -= pos;
}

static void receive(int ms) {
	struct pollfd p = { sock, POLLIN, 0 };
	if (poll(&p, 1, ms) <= 0) return;
	ssize_t n = read(sock, in + inlen, sizeof in - inlen);
	if (n <= 0) die("the compositor hung up");
	inlen += n;
	events();
}

int main(int argc, char **argv) {
	setvbuf(stdout, NULL, _IONBF, 0);
	int want_w = 0, want_h = 0;
	if (argc < 2 || sscanf(argv[1], "%dx%d", &want_w, &want_h) != 2) die("usage: wl-output-resize WxH [seconds]");
	double limit = argc > 2 ? atof(argv[2]) : 20;
	struct sockaddr_un sa = { .sun_family = AF_UNIX };
	const char *dir = getenv("XDG_RUNTIME_DIR");
	snprintf(sa.sun_path, sizeof sa.sun_path, "%s/wayland-0", dir ? dir : "/tmp");
	sock = socket(AF_UNIX, SOCK_STREAM, 0);
	if (connect(sock, (struct sockaddr *)&sa, sizeof sa)) die("no compositor at $XDG_RUNTIME_DIR/wayland-0");
	begin(1, 1); /* wl_display.get_registry */
	put(2);
	end();
	begin(1, 0); /* wl_display.sync */
	put(3);
	end();
	flush();
	double t0 = now_s();
	while (!(output_name && seat_name) && now_s() - t0 < limit) receive(200);
	if (!output_name || !seat_name) die("no wl_output or wl_seat");
	begin(2, 0); /* wl_registry.bind */
	put(output_name);
	put_str("wl_output");
	put(2);
	put(OUTPUT);
	end();
	begin(2, 0);
	put(seat_name);
	put_str("wl_seat");
	put(1);
	put(SEAT);
	end();
	if (xdg_name) {
		begin(2, 0);
		put(xdg_name);
		put_str("zxdg_output_manager_v1");
		put(3);
		put(XDG_MANAGER);
		end();
		begin(XDG_MANAGER, 1); /* get_xdg_output(id, output) */
		put(XDG_OUTPUT);
		put(OUTPUT);
		end();
	}
	begin(SEAT, 0); /* wl_seat.get_pointer */
	put(POINTER);
	end();
	begin(1, 0); /* wl_display.sync */
	put(SYNC);
	end();
	flush();
	while (!synced && now_s() - t0 < limit) receive(200);
	if (!bound) die("the bind told no size");
	if (xdg_name && !log_shown_w) die("the xdg_output told no logical size");
	/* Connected: from here every size is a resize. Polls that time out are
	 * turns of their own, as a client's timers are; the page wakes a process
	 * whose threads all wait. */
	while (now_s() - t0 < limit) {
		if (shown_w == want_w && shown_h == want_h && bound &&
		    (!xdg_name || (log_shown_w == want_w && log_shown_h == want_h)))
			return 0;
		receive(1000);
	}
	printf("wl-output-resize: no %dx%d in %.0f s\n", want_w, want_h, limit);
	return 1;
}
