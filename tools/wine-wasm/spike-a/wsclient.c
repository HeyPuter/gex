/* W0(a): a minimal wineserver client, the part of ntdll.so's
 * server_init_process that talks to the server: connect to the master
 * socket, receive the request fd and protocol version (SCM_RIGHTS), send
 * the reply and wait fds back, then init_first_thread, create_event,
 * event_op, get_handle_fd-free requests, and a timed loop of a cheap
 * request to measure the round trip.
 *
 *   wsclient SERVER_DIR [N [WINESERVER]]
 * where SERVER_DIR is /tmp/.wine-UID/server-DEV-INO (wineserver's). With
 * WINESERVER, start it first (posix_spawn, as ntdll's start_server forks
 * and execs it) and wait for its socket. */
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <time.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/uio.h>
#include <spawn.h>
#include <sys/stat.h>
#include "windef.h"
#include "winbase.h"
#include "wine/server_protocol.h"

static int fd_socket, request_fd, reply_fd;

static void die(const char *m) { perror(m); exit(1); }

static int receive_fd(obj_handle_t *handle)
{
    struct iovec vec = { handle, sizeof(*handle) };
    char buf[256];
    struct msghdr mh = { 0 };
    mh.msg_iov = &vec; mh.msg_iovlen = 1; mh.msg_control = buf; mh.msg_controllen = sizeof(buf);
    int ret = recvmsg(fd_socket, &mh, 0), fd = -1;
    if (ret <= 0) die("recvmsg");
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&mh); c; c = CMSG_NXTHDR(&mh, c))
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) fd = *(int *)CMSG_DATA(c);
    return fd;
}

static void send_fd(int fd, thread_id_t tid)
{
    struct send_fd data = { tid, fd };
    struct iovec vec = { &data, sizeof(data) };
    char buf[256];
    struct msghdr mh = { 0 };
    mh.msg_iov = &vec; mh.msg_iovlen = 1; mh.msg_control = buf; mh.msg_controllen = sizeof(buf);
    struct cmsghdr *c = CMSG_FIRSTHDR(&mh);
    c->cmsg_len = CMSG_LEN(sizeof(fd)); c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS;
    *(int *)CMSG_DATA(c) = fd;
    mh.msg_controllen = c->cmsg_len;
    if (sendmsg(fd_socket, &mh, 0) != sizeof(data)) die("sendmsg");
}

static void readall(int fd, void *p, size_t n)
{
    while (n) { ssize_t r = read(fd, p, n); if (r <= 0) die("read reply"); p = (char *)p + r; n -= r; }
}

/* One request: fixed part, optional variable data, reply (+ data). */
static unsigned int call(union generic_request *req, enum request code, const void *data, data_size_t dsize,
                         union generic_reply *rep, void *rdata, data_size_t rsize)
{
    req->request_header.req = code;
    req->request_header.request_size = dsize;
    req->request_header.reply_size = rsize;
    struct iovec v[2] = { { req, sizeof(*req) }, { (void *)data, dsize } };
    if (writev(request_fd, v, dsize ? 2 : 1) != (ssize_t)(sizeof(*req) + dsize)) die("writev request");
    readall(reply_fd, rep, sizeof(*rep));
    if (rep->reply_header.reply_size) readall(reply_fd, rdata, rep->reply_header.reply_size);
    return rep->reply_header.error;
}

static double now(void)
{
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec * 1e-9;
}

int main(int argc, char **argv)
{
    if (argc < 2) { fprintf(stderr, "usage: wsclient SERVER_DIR [N]\n"); return 2; }
    int n = argc > 2 ? atoi(argv[2]) : 10000;
    if (argc > 3) {
        extern char **environ;
        char *sargv[] = { argv[3], "-f", "-p2", NULL };
        pid_t pid;
        int e = posix_spawn(&pid, argv[3], NULL, NULL, sargv, environ);
        if (e) { errno = e; die("posix_spawn wineserver"); }
        printf("spawned wineserver, pid %d\n", pid);
        char sock[512];
        snprintf(sock, sizeof(sock), "%s/socket", argv[1]);
        struct stat st;
        for (int i = 0; i < 2000 && stat(sock, &st); i++) usleep(5000);
    }
    if (chdir(argv[1])) die("chdir server dir");
    struct sockaddr_un addr = { AF_UNIX, "socket" };
    if ((fd_socket = socket(AF_UNIX, SOCK_STREAM, 0)) < 0) die("socket");
    if (connect(fd_socket, (struct sockaddr *)&addr, sizeof(addr))) die("connect");

    obj_handle_t version;
    request_fd = receive_fd(&version);
    printf("connected: protocol %u (client %u), request fd %d\n", version, SERVER_PROTOCOL_VERSION, request_fd);
    if (version != SERVER_PROTOCOL_VERSION) return 1;

    int reply_pipe[2], wait_pipe[2];
    if (pipe(reply_pipe) || pipe(wait_pipe)) die("pipe");
    send_fd(reply_pipe[1], 0);
    send_fd(wait_pipe[1], 0);
    reply_fd = reply_pipe[0];

    union generic_request req;
    union generic_reply rep;
    unsigned short machines[8];
    memset(&req, 0, sizeof(req));
    req.init_first_thread_request.unix_pid = getpid();
    req.init_first_thread_request.unix_tid = getpid();
    req.init_first_thread_request.reply_fd = reply_pipe[1];
    req.init_first_thread_request.wait_fd = wait_pipe[1];
    req.init_first_thread_request.page_size = 4096;
    unsigned int st = call(&req, REQ_init_first_thread, NULL, 0, &rep, machines, sizeof(machines));
    printf("init_first_thread: status %#x pid %04x tid %04x session %u machines %u (first %#x)\n", st,
           rep.init_first_thread_reply.pid, rep.init_first_thread_reply.tid,
           rep.init_first_thread_reply.session_id,
           rep.reply_header.reply_size / 2, machines[0]);
    if (st) return 1;

    /* an event object: create, set, query */
    memset(&req, 0, sizeof(req));
    req.create_event_request.access = EVENT_ALL_ACCESS;
    req.create_event_request.manual_reset = 1;
    req.create_event_request.initial_state = 0;
    st = call(&req, REQ_create_event, NULL, 0, &rep, NULL, 0);
    obj_handle_t ev = rep.create_event_reply.handle;
    printf("create_event: status %#x handle %#x\n", st, ev);
    memset(&req, 0, sizeof(req));
    req.event_op_request.handle = ev;
    req.event_op_request.op = SET_EVENT;
    st = call(&req, REQ_event_op, NULL, 0, &rep, NULL, 0);
    printf("event_op(SET): status %#x previous %d\n", st, rep.event_op_reply.state);
    memset(&req, 0, sizeof(req));
    req.query_event_request.handle = ev;
    st = call(&req, REQ_query_event, NULL, 0, &rep, NULL, 0);
    printf("query_event: status %#x state %d manual %d\n", st, rep.query_event_reply.state, rep.query_event_reply.manual_reset);

    /* the round trip: n x event_op(PULSE) */
    double t0 = now();
    for (int i = 0; i < n; i++) {
        memset(&req, 0, sizeof(req));
        req.event_op_request.handle = ev;
        req.event_op_request.op = PULSE_EVENT;
        if (call(&req, REQ_event_op, NULL, 0, &rep, NULL, 0)) { printf("event_op failed\n"); return 1; }
    }
    double dt = now() - t0;
    printf("round trip: %d requests in %.3f s, %.1f us each\n", n, dt, dt / n * 1e6);
    return 0;
}
