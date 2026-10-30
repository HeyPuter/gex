/*
 * gex-host: a guest's call to the page (web/runtime System `hostCalls`).
 *
 * `gex-host NAME [JSON]` (JSON from standard input when not given, or "-")
 * connects to "\0gex-host", which the runtime Worker
 * (web/runtime/src/io-worker.js) listens on in the global kernel, sends a
 * u32 length (little-endian) and "NAME\0JSON", and reads back a u32 length,
 * a status byte (0 done, else failed) and the answer: on standard output
 * and exit 0, or on standard error and exit 1.
 */
#define _GNU_SOURCE
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static int host_call(int argc, char **argv) {
  if (argc < 2 || argc > 3) {
    fprintf(stderr, "usage: gex-host NAME [JSON]\n");
    return 2;
  }
  size_t nlen = strlen(argv[1]), jlen = 0, cap = 4096;
  char *json = NULL;
  if (argc == 3 && strcmp(argv[2], "-")) {
    json = argv[2];
    jlen = strlen(json);
  } else {
    json = malloc(cap);
    ssize_t r;
    while (json && (r = read(0, json + jlen, cap - jlen)) > 0) {
      jlen += (size_t)r;
      if (jlen == cap) json = realloc(json, cap *= 2);
    }
    if (!json) return 1;
  }
  struct sockaddr_un a;
  memset(&a, 0, sizeof a);
  a.sun_family = AF_UNIX;
  memcpy(a.sun_path + 1, "gex-host", 8);
  int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (s < 0 || connect(s, (struct sockaddr *)&a, (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 9)) < 0) {
    fprintf(stderr, "gex-host: the page takes no calls\n");
    return 1;
  }
  uint32_t len = (uint32_t)(nlen + 1 + jlen);
  unsigned char *msg = malloc(4 + len);
  memcpy(msg, &len, 4);
  memcpy(msg + 4, argv[1], nlen + 1);
  memcpy(msg + 5 + nlen, json, jlen);
  for (size_t at = 0; at < 4 + len;) {
    ssize_t w = write(s, msg + at, 4 + len - at);
    if (w <= 0) return 1;
    at += (size_t)w;
  }
  unsigned char head[5];
  for (size_t at = 0; at < 5;) {
    ssize_t r = read(s, head + at, 5 - at);
    if (r <= 0) {
      fprintf(stderr, "gex-host: no answer\n");
      return 1;
    }
    at += (size_t)r;
  }
  uint32_t n;
  memcpy(&n, head, 4);
  int fd = head[4] ? 2 : 1;
  char buf[8192];
  for (uint32_t got = 0; got < n;) {
    ssize_t r = read(s, buf, n - got < sizeof buf ? n - got : sizeof buf);
    if (r <= 0) return 1;
    for (ssize_t w = 0; w < r;) {
      ssize_t k = write(fd, buf + w, (size_t)(r - w));
      if (k <= 0) return 1;
      w += k;
    }
    got += (uint32_t)r;
  }
  (void)!write(fd, "\n", 1);
  return head[4] ? 1 : 0;
}

int main(int argc, char **argv) {
  return host_call(argc, argv);
}
