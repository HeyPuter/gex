/* /dev/gex-gpu, the gex host's WebGPU device: its ioctls. The host's
 * halves are shims/native/src/gpu.rs and shims/browser/src/gpu.rs. */

#ifndef GEXGPU_TRANSPORT_DEV_H
#define GEXGPU_TRANSPORT_DEV_H

#include <stdint.h>

/* One message and where its reply goes; addresses in the caller's memory. */
struct gex_gpu_submit
{
    uint64_t data, size;
    uint64_t reply, reply_capacity;
    /* Out: the reply's length, which is more than reply_capacity when
     * status is GEX_GPU_STATUS_HELD. */
    uint64_t reply_size;
    uint32_t status;
    /* GEXGPU_WIRE_VERSION. */
    uint32_t version;
};

enum gex_gpu_status
{
    GEX_GPU_STATUS_OK = 0,
    /* The reply did not fit; the device holds it for GEX_GPU_FETCH. */
    GEX_GPU_STATUS_HELD = 1,
    /* The host could not parse the message (what it ran is replied to). */
    GEX_GPU_STATUS_MALFORMED = 2,
};

/* _IOWR('w', 1, struct gex_gpu_submit): run a message. */
#define GEX_GPU_SUBMIT 0xc0307701u
/* _IOWR('w', 2, struct gex_gpu_submit): take the held reply (data and size
 * are unused). */
#define GEX_GPU_FETCH 0xc0307702u

#endif
