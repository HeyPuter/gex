/* The wire's transport in a gex guest: each message goes to the host through
 * /dev/gex-gpu, whose ioctl runs it on the host's webgpu.h and answers in
 * place. The device reads the message from guest memory where it is, so a
 * message is one host call however many commands it holds.
 *
 * Every open of the device is a connection of its own (the ids are the
 * client's), so a process opens it once, here. */

#if 0
#pragma makedep unix
#endif

#include <fcntl.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include "wire.h"
#include "transport-dev.h"

extern const void *(*gexgpu_client_transport)(void *context, const void *data, size_t size, size_t *reply_size);

static int device = -1;
static uint8_t *reply;
static size_t reply_capacity;

static int submit(unsigned long request, const void *data, size_t size, struct gex_gpu_submit *s)
{
    memset(s, 0, sizeof(*s));
    s->data = (uintptr_t)data;
    s->size = size;
    s->reply = (uintptr_t)reply;
    s->reply_capacity = reply_capacity;
    s->version = GEXGPU_WIRE_VERSION;
    return ioctl(device, request, s);
}

static const void *transport(void *context, const void *data, size_t size, size_t *reply_size)
{
    struct gex_gpu_submit s;

    *reply_size = 0;
    if (submit(GEX_GPU_SUBMIT, data, size, &s) < 0)
    {
        perror("gexgpu: /dev/gex-gpu");
        return NULL;
    }
    /* A reply bigger than the buffer waits on the host for a bigger one. */
    if (s.status == GEX_GPU_STATUS_HELD)
    {
        size_t capacity = reply_capacity;
        uint8_t *p;

        while (capacity < s.reply_size)
            capacity *= 2;
        if (!(p = realloc(reply, capacity)))
            return NULL;
        reply = p;
        reply_capacity = capacity;
        if (submit(GEX_GPU_FETCH, NULL, 0, &s) < 0 || s.status == GEX_GPU_STATUS_HELD)
            return NULL;
    }
    if (s.status == GEX_GPU_STATUS_MALFORMED)
        fprintf(stderr, "gexgpu: the host rejected a message of %zu bytes\n", size);
    *reply_size = s.reply_size;
    return reply;
}

/* Without a device, every call fails as an unanswered one. */
static const void *no_transport(void *context, const void *data, size_t size, size_t *reply_size)
{
    *reply_size = 0;
    return NULL;
}

void gexgpu_client_init_transport(void)
{
    gexgpu_client_transport = no_transport;
    if ((device = open("/dev/gex-gpu", O_RDWR | O_CLOEXEC)) < 0)
    {
        perror("gexgpu: /dev/gex-gpu");
        return;
    }
    reply_capacity = 64 * 1024;
    if (!(reply = malloc(reply_capacity)))
        return;
    gexgpu_client_transport = transport;
}
