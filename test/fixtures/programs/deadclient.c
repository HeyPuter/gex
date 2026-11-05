/* deadclient: raw Wayland clients (no libwayland) for core/deadclient.
 * Four processes each map a 160x120 toplevel with a title of its own:
 *   dc-alive    this one, the parent, which stays up
 *   dc-killed   a child, killed with SIGKILL
 *   dc-trap     a child, which traps (__builtin_trap: a wasm trap when built
 *               for wasm32, so its Worker throws instead of exiting)
 *   dc-hangup   a child, which closes its connection and stays up
 * The parent prints "phase shown" once all four have mapped, waits 3 s, has
 * the children die or hang up, prints "phase gone" when both dead children
 * are reaped, waits 3 s more for the compositor, prints "phase after", then
 * "deadclient done", and stays up for 600 s. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int sock;
static uint32_t buf[256];
static int len; /* words in buf */

static void die(const char *why) {
	printf("deadclient %d: %s\n", (int)getpid(), why);
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

static void wl_bind(uint32_t name, const char *iface, uint32_t id) {
	begin(2, 0); /* wl_registry.bind */
	put(name);
	put_str(iface);
	put(1);
	put(id);
	end();
}

/* Connect, map a toplevel titled `title` in `color`, and wait for the
 * compositor to have it (a wl_display.sync after the commit). */
static void map_window(const char *title, uint32_t color) {
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
	 * 10 toplevel, 11 buffer, 12 the last sync's callback */
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
	begin(10, 2); /* xdg_toplevel.set_title */
	put_str(title);
	end();
	begin(7, 6); /* wl_surface.commit, for the first configure */
	end();
	flush(-1);
	pump_until(&configure_serial);
	begin(9, 4); /* xdg_surface.ack_configure */
	put(configure_serial & 0x7fffffffu);
	end();
	const int W = 160, H = 120, size = W * H * 4;
	int fd = memfd_create("deadclient", 0);
	if (fd < 0 || ftruncate(fd, size)) die("memfd");
	uint32_t *px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (px == MAP_FAILED) die("mmap");
	for (int i = 0; i < W * H; i++) px[i] = color;
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
	callback_id = 12;
	done = 0;
	begin(1, 0); /* wl_display.sync */
	put(12);
	end();
	flush(-1);
	pump_until(&done);
}

/* A child that maps `title`, says so on `ready`, waits for a byte on `go`,
 * then does what `how` says: 'p' pause (to be killed), 't' trap, 'h' close
 * its connection and stay up. */
static __attribute__((noinline)) void trap_now(void) { __builtin_trap(); }
static pid_t child(const char *title, uint32_t color, char how, int ready, int *go_w) {
	int go[2];
	if (pipe(go)) die("pipe");
	pid_t pid = fork();
	if (pid < 0) die("fork");
	if (pid) {
		close(go[0]);
		*go_w = go[1];
		return pid;
	}
	close(go[1]);
	map_window(title, color);
	printf("mapped %s\n", title);
	if (write(ready, "r", 1) != 1) die("ready");
	char c;
	if (read(go[0], &c, 1) != 1) die("go");
	if (how == 't') trap_now();
	if (how == 'h') {
		close(sock);
		printf("hung up %s\n", title);
	}
	for (;;) pause();
}

int main(void) {
	setvbuf(stdout, NULL, _IONBF, 0);
	int ready[2];
	if (pipe(ready)) die("pipe");
	int go_k, go_t, go_h;
	pid_t killed = child("dc-killed", 0xffc03030, 'p', ready[1], &go_k);
	pid_t trapped = child("dc-trap", 0xff30c030, 't', ready[1], &go_t);
	pid_t hungup = child("dc-hangup", 0xffc0c030, 'h', ready[1], &go_h);
	map_window("dc-alive", 0xff3070c0);
	printf("mapped dc-alive\n");
	for (int n = 0; n < 3; n++) {
		char c;
		if (read(ready[0], &c, 1) != 1) die("a child did not map");
	}
	sleep(1); /* for the page's compositor to have drawn them */
	printf("phase shown\n");
	sleep(3);
	if (kill(killed, SIGKILL)) die("kill");
	if (write(go_t, "g", 1) != 1 || write(go_h, "g", 1) != 1) die("go");
	int st;
	if (waitpid(killed, &st, 0) != killed) die("waitpid dc-killed");
	printf("dc-killed status %#x\n", st);
	if (waitpid(trapped, &st, 0) != trapped) die("waitpid dc-trap");
	printf("dc-trap status %#x\n", st);
	printf("phase gone\n");
	sleep(3);
	printf("phase after\n");
	printf("deadclient done\n");
	sleep(600);
	(void)hungup;
	return 0;
}
