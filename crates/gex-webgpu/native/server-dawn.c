/* The wire's server on the webgpu.h this is linked against (Dawn). */

#include "wire.h"
#include "wire-server.h"

struct gexgpu_server *gexgpu_server_create_dawn(void)
{
    struct gexgpu_procs procs;

    GEXGPU_PROCS_INIT_DIRECT(&procs);
    return gexgpu_server_create(&procs);
}
