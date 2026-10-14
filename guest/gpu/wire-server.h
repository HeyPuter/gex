/* The gex WebGPU wire's server (wire-server.c). */

#ifndef GEXGPU_WIRE_SERVER_H
#define GEXGPU_WIRE_SERVER_H

struct gexgpu_server;

/* A server making its client's calls on "procs" (GEXGPU_PROCS_INIT_DIRECT
 * fills them with the webgpu.h the caller is compiled against). */
struct gexgpu_server *gexgpu_server_create(const struct gexgpu_procs *procs);

/* Releases everything the client made and frees the server. */
void gexgpu_server_destroy(struct gexgpu_server *server);

/* Handles a message of commands; the reply (call results, events) is valid
 * until the next call. Returns false for a malformed message. */
bool gexgpu_server_handle(struct gexgpu_server *server, const void *data, size_t size,
        const void **reply, size_t *reply_size);

#endif
