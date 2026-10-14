/* The server half of the gex WebGPU wire: makes a client's commands on a
 * real webgpu.h, given as a proc table (wire.h).
 *
 * Most commands are generated (gen/wire-server-gen.inc). Those here have
 * callbacks: the server makes the call with its own callback, which puts
 * the result in an event record for the client, under the client's cookie.
 * Its callbacks are AllowProcessEvents, so they fire in WaitAny and
 * ProcessEvents, which the client's WAIT and PROCESS_EVENTS commands make. */

/* In a Wine DLL, the server is Unix-side code. */
#if 0
#pragma makedep unix
#endif

#include "wire.h"
#include "wire-server.h"

struct gexgpu_arena_chunk
{
    struct gexgpu_arena_chunk *next;
    size_t size, used;
    uint8_t data[];
};

struct gexgpu_server_object
{
    void *object;
    uint32_t type;
};

struct gexgpu_server_future
{
    uint32_t cookie;
    WGPUFuture future;
};

struct gexgpu_server
{
    struct gexgpu_procs procs;
    WGPUInstance instance;
    struct gexgpu_server_object *objects;
    uint32_t object_capacity;
    struct gexgpu_server_future *futures;
    uint32_t future_count, future_capacity;
    /* The reply being built, and events since the last one. */
    struct gexgpu_writer reply, events;
    size_t reply_start;
    struct gexgpu_arena_chunk *arena;
};

/* The server a message is for, while it is handled: deserialisers look up
 * objects in it. */
static struct gexgpu_server *current;

static void *arena_alloc(struct gexgpu_reader *r, size_t size)
{
    struct gexgpu_server *server = r->alloc_context;
    struct gexgpu_arena_chunk *chunk = server->arena;

    size = (size + 15) & ~(size_t)15;
    if (!chunk || chunk->size - chunk->used < size)
    {
        size_t chunk_size = size > 65536 ? size : 65536;

        if (!(chunk = malloc(sizeof(*chunk) + chunk_size)))
            return NULL;
        chunk->next = server->arena;
        chunk->size = chunk_size;
        chunk->used = 0;
        server->arena = chunk;
    }
    chunk->used += size;
    return memset(chunk->data + chunk->used - size, 0, size);
}

static void arena_reset(struct gexgpu_server *server)
{
    struct gexgpu_arena_chunk *chunk, *next;

    for (chunk = server->arena; chunk; chunk = next)
    {
        next = chunk->next;
        free(chunk);
    }
    server->arena = NULL;
}

static void *gexgpu_r_object(struct gexgpu_reader *r)
{
    uint32_t id = gexgpu_r_u32(r);

    if (!id)
        return NULL;
    if (id >= current->object_capacity || !current->objects[id].object)
    {
        r->error = true;
        return NULL;
    }
    return current->objects[id].object;
}

static void gexgpu_server_set_object(struct gexgpu_server *server, uint32_t id, uint32_t type, void *object)
{
    if (id >= server->object_capacity)
    {
        uint32_t capacity = server->object_capacity ? server->object_capacity : 1024;
        struct gexgpu_server_object *objects;

        while (capacity <= id)
            capacity *= 2;
        if (!(objects = realloc(server->objects, capacity * sizeof(*objects))))
            abort();
        memset(objects + server->object_capacity, 0, (capacity - server->object_capacity) * sizeof(*objects));
        server->objects = objects;
        server->object_capacity = capacity;
    }
    server->objects[id].object = object;
    server->objects[id].type = type;
}

static void gexgpu_server_reply_done(struct gexgpu_server *server)
{
    gexgpu_w_patch_length(&server->reply, server->reply_start);
}

static void reply_begin(struct gexgpu_server *server)
{
    server->reply_start = server->reply.size;
    gexgpu_w_u32(&server->reply, GEXGPU_RECORD_REPLY);
    gexgpu_w_u32(&server->reply, 0);
}

/* An event record for "cookie": the caller writes the callback's arguments
 * to the returned writer, then ends it. */
static size_t event_begin(struct gexgpu_server *server, uint32_t cookie)
{
    size_t start = server->events.size;

    gexgpu_w_u32(&server->events, GEXGPU_RECORD_EVENT);
    gexgpu_w_u32(&server->events, 0);
    gexgpu_w_u32(&server->events, cookie);
    return start;
}

static void event_end(struct gexgpu_server *server, size_t start)
{
    gexgpu_w_patch_length(&server->events, start);
}

static void add_future(struct gexgpu_server *server, uint32_t cookie, WGPUFuture future)
{
    if (server->future_count == server->future_capacity)
    {
        uint32_t capacity = server->future_capacity ? server->future_capacity * 2 : 64;
        struct gexgpu_server_future *futures;

        if (!(futures = realloc(server->futures, capacity * sizeof(*futures))))
            abort();
        server->futures = futures;
        server->future_capacity = capacity;
    }
    server->futures[server->future_count].cookie = cookie;
    server->futures[server->future_count].future = future;
    ++server->future_count;
}

static void remove_future(struct gexgpu_server *server, uint32_t cookie)
{
    uint32_t i;

    for (i = 0; i < server->future_count; ++i)
    {
        if (server->futures[i].cookie == cookie)
        {
            server->futures[i] = server->futures[--server->future_count];
            return;
        }
    }
}

static uint32_t cookie_of(void *userdata)
{
    return (uint32_t)(uintptr_t)userdata;
}

#define COOKIE(cookie) ((void *)(uintptr_t)(cookie))

/* ---- callbacks -------------------------------------------------------------------------- */

struct request_context
{
    struct gexgpu_server *server;
    uint32_t cookie, id, queue_id;
};

static void request_adapter_callback(WGPURequestAdapterStatus status, WGPUAdapter adapter,
        WGPUStringView message, void *userdata1, void *userdata2)
{
    struct request_context *c = userdata1;
    size_t start;

    if (status == WGPURequestAdapterStatus_Success)
        gexgpu_server_set_object(c->server, c->id, GEXGPU_TYPE_Adapter, adapter);
    start = event_begin(c->server, c->cookie);
    gexgpu_w_u32(&c->server->events, status);
    gexgpu_w_string(&c->server->events, message);
    event_end(c->server, start);
    remove_future(c->server, c->cookie);
    free(c);
}

static void request_device_callback(WGPURequestDeviceStatus status, WGPUDevice device,
        WGPUStringView message, void *userdata1, void *userdata2)
{
    struct request_context *c = userdata1;
    size_t start;

    if (status == WGPURequestDeviceStatus_Success)
    {
        gexgpu_server_set_object(c->server, c->id, GEXGPU_TYPE_Device, device);
        gexgpu_server_set_object(c->server, c->queue_id, GEXGPU_TYPE_Queue, c->server->procs.deviceGetQueue(device));
    }
    start = event_begin(c->server, c->cookie);
    gexgpu_w_u32(&c->server->events, status);
    gexgpu_w_string(&c->server->events, message);
    event_end(c->server, start);
    remove_future(c->server, c->cookie);
    free(c);
}

static void device_lost_callback(WGPUDevice const *device, WGPUDeviceLostReason reason,
        WGPUStringView message, void *userdata1, void *userdata2)
{
    struct gexgpu_server *server = userdata1;
    size_t start;

    start = event_begin(server, cookie_of(userdata2));
    gexgpu_w_u32(&server->events, reason);
    gexgpu_w_string(&server->events, message);
    event_end(server, start);
}

static void uncaptured_error_callback(WGPUDevice const *device, WGPUErrorType type,
        WGPUStringView message, void *userdata1, void *userdata2)
{
    struct gexgpu_server *server = userdata1;
    size_t start;

    start = event_begin(server, cookie_of(userdata2));
    gexgpu_w_u32(&server->events, type);
    gexgpu_w_string(&server->events, message);
    event_end(server, start);
}

struct map_context
{
    struct gexgpu_server *server;
    WGPUBuffer buffer;
    uint32_t cookie;
    uint64_t offset, size;
};

static void map_callback(WGPUMapAsyncStatus status, WGPUStringView message, void *userdata1, void *userdata2)
{
    struct map_context *c = userdata1;
    const void *data = NULL;
    size_t start;

    if (status == WGPUMapAsyncStatus_Success)
        data = c->server->procs.bufferGetConstMappedRange(c->buffer, c->offset, c->size);
    start = event_begin(c->server, c->cookie);
    gexgpu_w_u32(&c->server->events, status);
    gexgpu_w_string(&c->server->events, message);
    /* The mapped data comes with the event, for reading and writing both:
     * a write mapping starts with the buffer's contents. */
    gexgpu_w_blob(&c->server->events, data, data ? c->size : 0);
    event_end(c->server, start);
    remove_future(c->server, c->cookie);
    c->server->procs.bufferRelease(c->buffer);
    free(c);
}

static void work_done_callback(WGPUQueueWorkDoneStatus status, WGPUStringView message,
        void *userdata1, void *userdata2)
{
    struct gexgpu_server *server = userdata1;
    uint32_t cookie = cookie_of(userdata2);
    size_t start;

    start = event_begin(server, cookie);
    gexgpu_w_u32(&server->events, status);
    gexgpu_w_string(&server->events, message);
    event_end(server, start);
    remove_future(server, cookie);
}

static void compilation_info_callback(WGPUCompilationInfoRequestStatus status, WGPUCompilationInfo const *info,
        void *userdata1, void *userdata2)
{
    struct gexgpu_server *server = userdata1;
    uint32_t cookie = cookie_of(userdata2);
    size_t i, start;

    start = event_begin(server, cookie);
    gexgpu_w_u32(&server->events, status);
    gexgpu_w_u32(&server->events, info ? (uint32_t)info->messageCount : 0);
    for (i = 0; info && i < info->messageCount; ++i)
    {
        const WGPUCompilationMessage *m = &info->messages[i];

        gexgpu_w_string(&server->events, m->message);
        gexgpu_w_u32(&server->events, m->type);
        gexgpu_w_u64(&server->events, m->lineNum);
        gexgpu_w_u64(&server->events, m->linePos);
        gexgpu_w_u64(&server->events, m->offset);
        gexgpu_w_u64(&server->events, m->length);
    }
    event_end(server, start);
    remove_future(server, cookie);
}

static void pop_error_scope_callback(WGPUPopErrorScopeStatus status, WGPUErrorType type,
        WGPUStringView message, void *userdata1, void *userdata2)
{
    struct gexgpu_server *server = userdata1;
    uint32_t cookie = cookie_of(userdata2);
    size_t start;

    start = event_begin(server, cookie);
    gexgpu_w_u32(&server->events, status);
    gexgpu_w_u32(&server->events, type);
    gexgpu_w_string(&server->events, message);
    event_end(server, start);
    remove_future(server, cookie);
}

static void create_compute_pipeline_callback(WGPUCreatePipelineAsyncStatus status, WGPUComputePipeline pipeline,
        WGPUStringView message, void *userdata1, void *userdata2)
{
    struct request_context *c = userdata1;
    size_t start;

    if (status == WGPUCreatePipelineAsyncStatus_Success)
        gexgpu_server_set_object(c->server, c->id, GEXGPU_TYPE_ComputePipeline, pipeline);
    start = event_begin(c->server, c->cookie);
    gexgpu_w_u32(&c->server->events, status);
    gexgpu_w_string(&c->server->events, message);
    event_end(c->server, start);
    remove_future(c->server, c->cookie);
    free(c);
}

static void create_render_pipeline_callback(WGPUCreatePipelineAsyncStatus status, WGPURenderPipeline pipeline,
        WGPUStringView message, void *userdata1, void *userdata2)
{
    struct request_context *c = userdata1;
    size_t start;

    if (status == WGPUCreatePipelineAsyncStatus_Success)
        gexgpu_server_set_object(c->server, c->id, GEXGPU_TYPE_RenderPipeline, pipeline);
    start = event_begin(c->server, c->cookie);
    gexgpu_w_u32(&c->server->events, status);
    gexgpu_w_string(&c->server->events, message);
    event_end(c->server, start);
    remove_future(c->server, c->cookie);
    free(c);
}

static struct request_context *request_context(struct gexgpu_server *server, uint32_t cookie,
        uint32_t id, uint32_t queue_id)
{
    struct request_context *c;

    if (!(c = malloc(sizeof(*c))))
        abort();
    c->server = server;
    c->cookie = cookie;
    c->id = id;
    c->queue_id = queue_id;
    return c;
}

/* ---- dispatch --------------------------------------------------------------------------- */

static bool gexgpu_server_dispatch_handwritten(struct gexgpu_server *server, uint32_t op,
        struct gexgpu_reader *r);

#include "gen/wire-server-gen.inc"

static bool gexgpu_server_dispatch_handwritten(struct gexgpu_server *server, uint32_t op,
        struct gexgpu_reader *r)
{
    switch (op)
    {
        case GEXGPU_OP_CreateInstance:
        {
            static const WGPUInstanceFeatureName features[] = {WGPUInstanceFeatureName_TimedWaitAny};
            WGPUInstanceDescriptor desc = WGPU_INSTANCE_DESCRIPTOR_INIT;
            uint32_t id = gexgpu_r_u32(r);

            if (r->error)
                return false;
            /* One real instance serves the client's. */
            if (!server->instance)
            {
                desc.requiredFeatureCount = 1;
                desc.requiredFeatures = features;
                server->instance = server->procs.createInstance(&desc);
            }
            else
            {
                server->procs.instanceAddRef(server->instance);
            }
            gexgpu_server_set_object(server, id, GEXGPU_TYPE_Instance, server->instance);
            return true;
        }

        case GEXGPU_OP_InstanceRequestAdapter:
        {
            WGPURequestAdapterCallbackInfo info = WGPU_REQUEST_ADAPTER_CALLBACK_INFO_INIT;
            WGPUInstance instance = gexgpu_r_object(r);
            WGPURequestAdapterOptions *options = NULL;
            uint32_t cookie, id;

            if (gexgpu_r_u32(r) && (options = gexgpu_r_alloc(r, sizeof(*options))))
                deser_WGPURequestAdapterOptions(r, options);
            cookie = gexgpu_r_u32(r);
            id = gexgpu_r_u32(r);
            if (r->error)
                return false;
            info.mode = WGPUCallbackMode_AllowProcessEvents;
            info.callback = request_adapter_callback;
            info.userdata1 = request_context(server, cookie, id, 0);
            add_future(server, cookie, server->procs.instanceRequestAdapter(instance, options, info));
            return true;
        }

        case GEXGPU_OP_AdapterRequestDevice:
        {
            WGPURequestDeviceCallbackInfo info = WGPU_REQUEST_DEVICE_CALLBACK_INFO_INIT;
            WGPUDeviceDescriptor *desc = NULL, empty = WGPU_DEVICE_DESCRIPTOR_INIT;
            uint32_t cookie, id, queue_id, lost_cookie, error_cookie;
            WGPUAdapter adapter = gexgpu_r_object(r);

            if (gexgpu_r_u32(r) && (desc = gexgpu_r_alloc(r, sizeof(*desc))))
                deser_WGPUDeviceDescriptor(r, desc);
            cookie = gexgpu_r_u32(r);
            id = gexgpu_r_u32(r);
            queue_id = gexgpu_r_u32(r);
            lost_cookie = gexgpu_r_u32(r);
            error_cookie = gexgpu_r_u32(r);
            if (r->error)
                return false;
            if (!desc)
                desc = &empty;
            desc->deviceLostCallbackInfo = (WGPUDeviceLostCallbackInfo)WGPU_DEVICE_LOST_CALLBACK_INFO_INIT;
            desc->deviceLostCallbackInfo.mode = WGPUCallbackMode_AllowProcessEvents;
            desc->deviceLostCallbackInfo.callback = device_lost_callback;
            desc->deviceLostCallbackInfo.userdata1 = server;
            desc->deviceLostCallbackInfo.userdata2 = COOKIE(lost_cookie);
            desc->uncapturedErrorCallbackInfo = (WGPUUncapturedErrorCallbackInfo)WGPU_UNCAPTURED_ERROR_CALLBACK_INFO_INIT;
            if (error_cookie)
            {
                desc->uncapturedErrorCallbackInfo.callback = uncaptured_error_callback;
                desc->uncapturedErrorCallbackInfo.userdata1 = server;
                desc->uncapturedErrorCallbackInfo.userdata2 = COOKIE(error_cookie);
            }
            info.mode = WGPUCallbackMode_AllowProcessEvents;
            info.callback = request_device_callback;
            info.userdata1 = request_context(server, cookie, id, queue_id);
            add_future(server, cookie, server->procs.adapterRequestDevice(adapter, desc, info));
            return true;
        }

        case GEXGPU_OP_DeviceCreateComputePipelineAsync:
        case GEXGPU_OP_DeviceCreateRenderPipelineAsync:
        {
            bool compute = op == GEXGPU_OP_DeviceCreateComputePipelineAsync;
            WGPUComputePipelineDescriptor *compute_desc = NULL;
            WGPURenderPipelineDescriptor *render_desc = NULL;
            WGPUDevice device = gexgpu_r_object(r);
            uint32_t cookie, id;

            if (gexgpu_r_u32(r))
            {
                if (compute && (compute_desc = gexgpu_r_alloc(r, sizeof(*compute_desc))))
                    deser_WGPUComputePipelineDescriptor(r, compute_desc);
                else if (!compute && (render_desc = gexgpu_r_alloc(r, sizeof(*render_desc))))
                    deser_WGPURenderPipelineDescriptor(r, render_desc);
            }
            cookie = gexgpu_r_u32(r);
            id = gexgpu_r_u32(r);
            if (r->error)
                return false;
            if (compute)
            {
                WGPUCreateComputePipelineAsyncCallbackInfo info = WGPU_CREATE_COMPUTE_PIPELINE_ASYNC_CALLBACK_INFO_INIT;

                info.mode = WGPUCallbackMode_AllowProcessEvents;
                info.callback = create_compute_pipeline_callback;
                info.userdata1 = request_context(server, cookie, id, 0);
                add_future(server, cookie, server->procs.deviceCreateComputePipelineAsync(device, compute_desc, info));
            }
            else
            {
                WGPUCreateRenderPipelineAsyncCallbackInfo info = WGPU_CREATE_RENDER_PIPELINE_ASYNC_CALLBACK_INFO_INIT;

                info.mode = WGPUCallbackMode_AllowProcessEvents;
                info.callback = create_render_pipeline_callback;
                info.userdata1 = request_context(server, cookie, id, 0);
                add_future(server, cookie, server->procs.deviceCreateRenderPipelineAsync(device, render_desc, info));
            }
            return true;
        }

        case GEXGPU_OP_DevicePopErrorScope:
        {
            WGPUPopErrorScopeCallbackInfo info = WGPU_POP_ERROR_SCOPE_CALLBACK_INFO_INIT;
            WGPUDevice device = gexgpu_r_object(r);
            uint32_t cookie = gexgpu_r_u32(r);

            if (r->error)
                return false;
            info.mode = WGPUCallbackMode_AllowProcessEvents;
            info.callback = pop_error_scope_callback;
            info.userdata1 = server;
            info.userdata2 = COOKIE(cookie);
            add_future(server, cookie, server->procs.devicePopErrorScope(device, info));
            return true;
        }

        case GEXGPU_OP_BufferMapAsync:
        {
            WGPUBufferMapCallbackInfo info = WGPU_BUFFER_MAP_CALLBACK_INFO_INIT;
            WGPUBuffer buffer = gexgpu_r_object(r);
            uint32_t cookie = gexgpu_r_u32(r);
            uint64_t mode = gexgpu_r_u64(r);
            uint64_t offset = gexgpu_r_u64(r);
            uint64_t size = gexgpu_r_u64(r);
            struct map_context *c;

            if (r->error)
                return false;
            if (!(c = malloc(sizeof(*c))))
                abort();
            c->server = server;
            c->buffer = buffer;
            c->cookie = cookie;
            c->offset = offset;
            c->size = size;
            server->procs.bufferAddRef(buffer);
            info.mode = WGPUCallbackMode_AllowProcessEvents;
            info.callback = map_callback;
            info.userdata1 = c;
            add_future(server, cookie, server->procs.bufferMapAsync(buffer, mode, offset, size, info));
            return true;
        }

        case GEXGPU_OP_QueueOnSubmittedWorkDone:
        {
            WGPUQueueWorkDoneCallbackInfo info = WGPU_QUEUE_WORK_DONE_CALLBACK_INFO_INIT;
            WGPUQueue queue = gexgpu_r_object(r);
            uint32_t cookie = gexgpu_r_u32(r);

            if (r->error)
                return false;
            info.mode = WGPUCallbackMode_AllowProcessEvents;
            info.callback = work_done_callback;
            info.userdata1 = server;
            info.userdata2 = COOKIE(cookie);
            add_future(server, cookie, server->procs.queueOnSubmittedWorkDone(queue, info));
            return true;
        }

        case GEXGPU_OP_ShaderModuleGetCompilationInfo:
        {
            WGPUCompilationInfoCallbackInfo info = WGPU_COMPILATION_INFO_CALLBACK_INFO_INIT;
            WGPUShaderModule module = gexgpu_r_object(r);
            uint32_t cookie = gexgpu_r_u32(r);

            if (r->error)
                return false;
            info.mode = WGPUCallbackMode_AllowProcessEvents;
            info.callback = compilation_info_callback;
            info.userdata1 = server;
            info.userdata2 = COOKIE(cookie);
            add_future(server, cookie, server->procs.shaderModuleGetCompilationInfo(module, info));
            return true;
        }

        case GEXGPU_OP_RELEASE:
        {
            uint32_t id = gexgpu_r_u32(r);

            if (r->error || id >= server->object_capacity || !server->objects[id].object)
                return false;
            gexgpu_server_release_object(server, server->objects[id].type, server->objects[id].object);
            server->objects[id].object = NULL;
            return true;
        }

        case GEXGPU_OP_BUFFER_UPDATE:
        {
            WGPUBuffer buffer = gexgpu_r_object(r);
            uint64_t offset = gexgpu_r_u64(r);
            void *data, *mapped;
            size_t size;

            data = gexgpu_r_blob(r, &size);
            if (r->error)
                return false;
            if (size && (mapped = server->procs.bufferGetMappedRange(buffer, offset, size)))
                memcpy(mapped, data, size);
            return true;
        }

        case GEXGPU_OP_WAIT:
        {
            WGPUFutureWaitInfo infos[64];
            uint64_t timeout = gexgpu_r_u64(r);
            uint32_t i, j, count = gexgpu_r_u32(r), n = 0;

            for (i = 0; i < count && !r->error; ++i)
            {
                uint32_t cookie = gexgpu_r_u32(r);

                for (j = 0; j < server->future_count && n < 64; ++j)
                {
                    if (server->futures[j].cookie == cookie)
                    {
                        infos[n].future = server->futures[j].future;
                        infos[n].completed = false;
                        ++n;
                    }
                }
            }
            if (r->error)
                return false;
            /* The callbacks the wait completes fire in it, and record their
             * events; any others pending fire in ProcessEvents. */
            if (n && server->instance)
                server->procs.instanceWaitAny(server->instance, n, infos, timeout);
            if (server->instance)
                server->procs.instanceProcessEvents(server->instance);
            reply_begin(server);
            gexgpu_server_reply_done(server);
            return true;
        }

        case GEXGPU_OP_PROCESS_EVENTS:
            if (server->instance)
                server->procs.instanceProcessEvents(server->instance);
            reply_begin(server);
            gexgpu_server_reply_done(server);
            return true;

        default:
            return false;
    }
}

struct gexgpu_server *gexgpu_server_create(const struct gexgpu_procs *procs)
{
    struct gexgpu_server *server;

    if (!(server = calloc(1, sizeof(*server))))
        return NULL;
    server->procs = *procs;
    return server;
}

void gexgpu_server_destroy(struct gexgpu_server *server)
{
    uint32_t id, instance_refs = 0;

    current = server;
    /* Everything but the instance first, so the callbacks the releases end
     * (device lost, pending maps) still find the server, and run in the
     * events processed after. */
    for (id = server->object_capacity; id--;)
    {
        struct gexgpu_server_object *o = &server->objects[id];

        if (!o->object)
            continue;
        if (o->type == GEXGPU_TYPE_Instance)
            ++instance_refs;
        else
            gexgpu_server_release_object(server, o->type, o->object);
        o->object = NULL;
    }
    if (server->instance)
    {
        server->procs.instanceProcessEvents(server->instance);
        while (instance_refs--)
            server->procs.instanceRelease(server->instance);
    }
    current = NULL;
    arena_reset(server);
    free(server->objects);
    free(server->futures);
    free(server->reply.data);
    free(server->events.data);
    free(server);
}

bool gexgpu_server_handle(struct gexgpu_server *server, const void *data, size_t size,
        const void **reply, size_t *reply_size)
{
    struct gexgpu_reader message = {.data = data, .size = size, .alloc = arena_alloc, .alloc_context = server};
    bool ok = true;

    current = server;
    server->reply.size = 0;

    while (message.pos < message.size)
    {
        uint32_t op = gexgpu_r_u32(&message);
        uint32_t length = gexgpu_r_u32(&message);
        struct gexgpu_reader r = message;
        bool sync;

        if (message.error || length < 8 || length - 8 > message.size - message.pos)
        {
            ok = false;
            break;
        }
        r.size = message.pos + length - 8;
        sync = op == GEXGPU_OP_WAIT || op == GEXGPU_OP_PROCESS_EVENTS;
        if (!sync)
            server->reply_start = server->reply.size;
        /* A generated call with a result writes its reply record itself. */
        if (op < GEXGPU_OP_COUNT && !sync)
        {
            size_t before = server->reply.size;

            reply_begin(server);
            if (!gexgpu_server_dispatch_gen(server, op, &r))
                ok = false;
            /* No result written: drop the empty record. */
            if (server->reply.size == before + 8)
                server->reply.size = before;
        }
        else if (!gexgpu_server_dispatch_handwritten(server, op, &r))
        {
            ok = false;
        }
        if (!ok)
            break;
        message.pos += length - 8;
        arena_reset(server);
    }
    arena_reset(server);

    /* Events after the reply: the client reads them all. */
    if (server->events.size)
    {
        gexgpu_w_raw(&server->reply, server->events.data, server->events.size);
        server->events.size = 0;
    }
    *reply = server->reply.data;
    *reply_size = server->reply.size;
    current = NULL;
    return ok;
}
