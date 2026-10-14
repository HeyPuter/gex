/* The wire's server in a page: wire-server.c on the browser's WebGPU
 * (emdawnwebgpu), for shims/browser/js/gpu-bridge.mjs. Built by
 * web/build-gexgpu-server.sh with JSPI, so a WAIT in a message suspends
 * gexgpu_web_handle's promise instead of the page. */

#include <emscripten.h>

#include "../wire.h"
#include "../wire-server.h"

static const void *reply;
static size_t reply_size;

EMSCRIPTEN_KEEPALIVE struct gexgpu_server *gexgpu_web_create(void)
{
    struct gexgpu_procs procs;

    GEXGPU_PROCS_INIT_DIRECT(&procs);
    return gexgpu_server_create(&procs);
}

EMSCRIPTEN_KEEPALIVE void gexgpu_web_destroy(struct gexgpu_server *server)
{
    gexgpu_server_destroy(server);
}

/* Runs a message; its reply is gexgpu_web_reply() until the next. */
EMSCRIPTEN_KEEPALIVE int gexgpu_web_handle(struct gexgpu_server *server, const void *data, size_t size)
{
    return gexgpu_server_handle(server, data, size, &reply, &reply_size);
}

EMSCRIPTEN_KEEPALIVE const void *gexgpu_web_reply(void)
{
    return reply;
}

EMSCRIPTEN_KEEPALIVE size_t gexgpu_web_reply_size(void)
{
    return reply_size;
}
