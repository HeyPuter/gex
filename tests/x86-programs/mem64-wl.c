/* A wl_shm pool above 4 GiB reaches the compositor whole
 * (tests/browser-wl-mem64.sh). A raw Wayland client (no libwayland), as
 * wl-unmap-pump.c: it maps a 64x64 toplevel from a pool mapped above 4 GiB,
 * fills it with a pattern no other memory holds, and waits for a
 * wl_display.sync to come back, by which time the compositor has published
 * the window. The surface's address used to be cut to 32 bits, so the page
 * drew whatever lived at the low alias -- the shim's heap -- or nothing.
 * Prints the pool's address and the pixels' sum (32-bit words added mod 2^32),
 * which the headless compositor's own sum (GEX_WL_STUB_SUM) must match. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static int sock;
static uint32_t buf[256];
static int len; /* words in buf */

static void die(const char *why) {
	printf("mem64-wl: %s\n", why);
	exit(1);
}

/* A request: object, opcode, then the words put after it. */
static int at;
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

static void flush(int fd) {
	struct iovec iov = { buf, (size_t)len * 4 };
	struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1 };
	char ctl[CMSG_SPACE(sizeof(int))];
	if (fd >= 0) {
		memset(ctl, 0, sizeof ctl);
		m.msg_control = ctl;
		m.msg_controllen = sizeof ctl;
		struct cmsghdr *c = CMSG_FIRSTHDR(&m);
		c->cmsg_level = SOL_SOCKET;
		c->cmsg_type = SCM_RIGHTS;
		c->cmsg_len = CMSG_LEN(sizeof(int));
		memcpy(CMSG_DATA(c), &fd, sizeof fd);
	}
	if (sendmsg(sock, &m, 0) != len * 4) die("sendmsg failed");
	len = 0;
}

static uint32_t inbuf[4096];
static int inlen, inpos; /* words */
static uint32_t compositor_name, shm_name, wm_name, callback_id, done, configure_serial;

/* Read events until `*until` is set. */
static void pump_until(uint32_t *until) {
	while (!*until) {
		if (inpos >= inlen) {
			ssize_t n = read(sock, inbuf, sizeof inbuf);
			if (n <= 0) die("the compositor hung up");
			inlen = n / 4;
			inpos = 0;
		}
		while (inpos + 2 <= inlen) {
			uint32_t obj = inbuf[inpos], op = inbuf[inpos + 1] & 0xffff, size = inbuf[inpos + 1] >> 16;
			uint32_t *a = &inbuf[inpos + 2];
			if (inpos + size / 4 > (uint32_t)inlen) die("a split event (not handled)");
			if (obj == 1 && op == 0) die("wl_display.error");
			if (obj == 2 && op == 0) { /* wl_registry.global */
				const char *iface = (const char *)&a[2];
				if (!strcmp(iface, "wl_compositor")) compositor_name = a[0];
				if (!strcmp(iface, "wl_shm")) shm_name = a[0];
				if (!strcmp(iface, "xdg_wm_base")) wm_name = a[0];
			}
			if (obj == callback_id && op == 0) done = 1;
			if (obj == 9 && op == 0) configure_serial = a[0] | 0x80000000u; /* xdg_surface.configure */
			inpos += size / 4;
		}
	}
}

static void sync_round(uint32_t id) {
	callback_id = id;
	done = 0;
	begin(1, 0); /* wl_display.sync */
	put(id);
	end();
	flush(-1);
	pump_until(&done);
}

static void wl_bind(uint32_t name, const char *iface, uint32_t id) {
	begin(2, 0); /* wl_registry.bind */
	put(name);
	put_str(iface);
	put(1);
	put(id);
	end();
}

int main(void) {
	setvbuf(stdout, NULL, _IONBF, 0);
	struct sockaddr_un sa = { .sun_family = AF_UNIX };
	const char *dir = getenv("XDG_RUNTIME_DIR");
	snprintf(sa.sun_path, sizeof sa.sun_path, "%s/wayland-0", dir ? dir : "/tmp");
	sock = socket(AF_UNIX, SOCK_STREAM, 0);
	if (connect(sock, (struct sockaddr *)&sa, sizeof sa)) die("no compositor at $XDG_RUNTIME_DIR/wayland-0");
	begin(1, 1); /* wl_display.get_registry */
	put(2);
	end();
	sync_round(3);
	if (!compositor_name || !shm_name || !wm_name) die("no wl_compositor, wl_shm or xdg_wm_base");
	/* 4 compositor, 5 shm, 6 wm base, 7 surface, 8 pool, 9 xdg_surface,
	 * 10 toplevel, 11 buffer */
	wl_bind(compositor_name, "wl_compositor", 4);
	wl_bind(shm_name, "wl_shm", 5);
	wl_bind(wm_name, "xdg_wm_base", 6);
	begin(4, 0); /* wl_compositor.create_surface */
	put(7);
	end();
	begin(6, 2); /* xdg_wm_base.get_xdg_surface */
	put(9);
	put(7);
	end();
	begin(9, 1); /* xdg_surface.get_toplevel */
	put(10);
	end();
	begin(7, 6); /* wl_surface.commit, for the first configure */
	end();
	flush(-1);
	pump_until(&configure_serial);
	begin(9, 4); /* xdg_surface.ack_configure */
	put(configure_serial & 0x7fffffffu);
	end();
	const int W = 64, H = 64, size = W * H * 4;
	int fd = memfd_create("mem64-wl", 0);
	if (fd < 0 || ftruncate(fd, size)) die("memfd");
	/* Above 4 GiB: asked for there, and taken wherever the kernel put it if
	 * it would not (natively, or with a wasm32 memory, the output is the
	 * same but for the address). */
	uint32_t *px = mmap((void *)0x140000000ul, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (px == MAP_FAILED) die("mmap");
	uint32_t sum = 0;
	for (int i = 0; i < W * H; i++) {
		px[i] = 0xff000000u | ((uint32_t)i * 2654435761u >> 8);
		sum += px[i];
	}
	printf("pool at %#lx\n", (unsigned long)px);
	printf("sum=%u\n", sum);
	begin(5, 0); /* wl_shm.create_pool, the fd alongside */
	put(8);
	put(size);
	end();
	flush(fd);
	begin(8, 0); /* wl_shm_pool.create_buffer: XRGB8888 */
	put(11);
	put(0);
	put(W);
	put(H);
	put(W * 4);
	put(1);
	end();
	begin(7, 1); /* wl_surface.attach */
	put(11);
	put(0);
	put(0);
	end();
	begin(7, 2); /* wl_surface.damage */
	put(0);
	put(0);
	put(W);
	put(H);
	end();
	begin(7, 6); /* wl_surface.commit */
	end();
	flush(-1);
	sync_round(12);
	/* A moment for the page to draw it, as a client that goes on would. */
	struct timespec ts = { 0, 300 * 1000 * 1000 };
	nanosleep(&ts, NULL);
	printf("mapped\n");
	return 0;
}
