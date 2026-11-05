/* repaint: a raw Wayland client (no libwayland) for core/repaint.
 * It maps a 640x480 toplevel from one wl_shm buffer, then commits the same
 * 8x400 damage rectangle ROUNDS times in each of two phases, one round
 * every 20 ms after a wl_display.sync:
 *   same     the rectangle's pixels unchanged
 *   change   the rectangle a different colour each round
 * and prints "phase NAME start" and "phase NAME end" around each, flushed,
 * then "repaint done" and stays up for 600 s. With --wait, it prints
 * "phase NAME ready" before each and starts it on a line from stdin: the
 * test's cue that the compositor has settled.
 *   repaint [ROUNDS] [--wait]   (default 60) */
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
	printf("repaint: %s\n", why);
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

/* A wl_display.sync on a fresh id each time (a callback is destroyed once
 * it is done). */
static uint32_t next_id = 12;
static void sync_round(void) {
	callback_id = next_id++;
	done = 0;
	begin(1, 0);
	put(callback_id);
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

static void commit_damage(int x, int y, int w, int h) {
	begin(7, 1); /* wl_surface.attach */
	put(11);
	put(0);
	put(0);
	end();
	begin(7, 2); /* wl_surface.damage */
	put(x);
	put(y);
	put(w);
	put(h);
	end();
	begin(7, 6); /* wl_surface.commit */
	end();
	flush(-1);
}

int main(int argc, char **argv) {
	setvbuf(stdout, NULL, _IONBF, 0);
	int rounds = argc > 1 ? atoi(argv[1]) : 60;
	int wait = argc > 2 && !strcmp(argv[2], "--wait");
	struct sockaddr_un sa = { .sun_family = AF_UNIX };
	const char *dir = getenv("XDG_RUNTIME_DIR");
	snprintf(sa.sun_path, sizeof sa.sun_path, "%s/wayland-0", dir ? dir : "/tmp");
	sock = socket(AF_UNIX, SOCK_STREAM, 0);
	if (connect(sock, (struct sockaddr *)&sa, sizeof sa)) die("no compositor at $XDG_RUNTIME_DIR/wayland-0");
	begin(1, 1); /* wl_display.get_registry */
	put(2);
	end();
	callback_id = 3;
	done = 0;
	begin(1, 0);
	put(3);
	end();
	flush(-1);
	pump_until(&done);
	if (!compositor_name || !shm_name || !wm_name) die("no wl_compositor, wl_shm or xdg_wm_base");
	/* 4 compositor, 5 shm, 6 wm base, 7 surface, 8 pool, 9 xdg_surface,
	 * 10 toplevel, 11 buffer, 12 on: sync callbacks */
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
	const int W = 640, H = 480, size = W * H * 4;
	int fd = memfd_create("repaint", 0);
	if (fd < 0 || ftruncate(fd, size)) die("memfd");
	uint32_t *px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (px == MAP_FAILED) die("mmap");
	for (int i = 0; i < W * H; i++) px[i] = 0xff3070c0;
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
	commit_damage(0, 0, W, H);
	sync_round();
	printf("window mapped\n");
	const int RX = 100, RY = 40, RW = 8, RH = 400;
	/* The rectangle before the phases, as damage of its own, until the
	 * compositor has uploaded it as such (a commit it saw after a gap is a
	 * whole-buffer upload): the frames that draws settled before anything
	 * is counted. */
	for (int i = 0; i < 3; i++) {
		commit_damage(RX, RY, RW, RH);
		sync_round();
		usleep(150 * 1000);
	}
	usleep(300 * 1000);
	static const char *names[2] = { "same", "change" };
	for (int phase = 0; phase < 2; phase++) {
		if (wait) {
			char line[16];
			printf("phase %s ready\n", names[phase]);
			if (!fgets(line, sizeof line, stdin)) die("stdin closed");
		}
		printf("phase %s start\n", names[phase]);
		for (int r = 0; r < rounds; r++) {
			if (phase == 1) {
				uint32_t c = 0xff000000u | (uint32_t)((r * 40 + 30) & 0xff) << 16 | 0x8040;
				for (int y = RY; y < RY + RH; y++)
					for (int x = RX; x < RX + RW; x++) px[y * W + x] = c;
			}
			commit_damage(RX, RY, RW, RH);
			sync_round();
			usleep(20 * 1000);
		}
		/* What the last commit asked for drawn before the phase ends. */
		usleep(300 * 1000);
		printf("phase %s end\n", names[phase]);
		usleep(200 * 1000);
	}
	printf("repaint done\n");
	sleep(600);
	return 0;
}
