/* The wire's client and server in one process, the server on the webgpu.h
 * this is linked against (Dawn): for testing the wire with programs written
 * to webgpu.h, compiled with gen/wire-rename-gen.h so their calls go to the
 * client. */

#include <stdio.h>

#include "../wire.h"
#include "../wire-server.h"

extern const void *(*gexgpu_client_transport)(void *context, const void *data, size_t size, size_t *reply_size);

static struct gexgpu_server *server;
static size_t messages, bytes;

static const void *transport(void *context, const void *data, size_t size, size_t *reply_size)
{
    const void *reply;

    ++messages;
    bytes += size;
    if (!gexgpu_server_handle(server, data, size, &reply, reply_size))
        fprintf(stderr, "gexgpu: the server rejected a message of %zu bytes\n", size);
    return reply;
}

static void report(void)
{
    if (getenv("GEXGPU_STATS"))
        fprintf(stderr, "gexgpu: %zu messages, %zu bytes\n", messages, bytes);
}

void gexgpu_client_init_transport(void)
{
    struct gexgpu_procs procs;

    GEXGPU_PROCS_INIT_DIRECT(&procs);
    server = gexgpu_server_create(&procs);
    gexgpu_client_transport = transport;
    atexit(report);
}
