/* The client half of the gex WebGPU wire: a webgpu.h whose calls become
 * commands for a server that makes them on a real one (wire.h).
 *
 * Most entry points are generated (gen/wire-client-gen.c). Those here are
 * the ones the client answers or tracks itself: callbacks, which come back
 * as events and fire in WaitAny/ProcessEvents as their mode allows; buffer
 * mappings, whose data the client holds a copy of (read data arrives with
 * the map's event, written data goes to the server at unmap); and getters
 * for what the client already knows (a texture's descriptor).
 *
 * The embedder gives the transport: gexgpu_client_transport sends a batch
 * of commands and returns the server's reply. In a Wine DLL that is a Unix
 * call; in the tests the server is in the same process. */

#include "wire.h"

#ifndef GEXGPU_CLIENT_NAME
# define GEXGPU_CLIENT_NAME(name) name
#endif

/* The embedder's: sends "size" bytes of commands, returns the reply (valid
 * until the next call) and its size. */
const void *(*gexgpu_client_transport)(void *context, const void *data, size_t size, size_t *reply_size);
void *gexgpu_client_transport_context;
/* Called once, before the first message, if no transport is set. */
void gexgpu_client_init_transport(void);

/* Batches are sent at this size even with no call waiting on them. */
#define GEXGPU_FLUSH_SIZE (256 * 1024)

struct gexgpu_object
{
    uint32_t id;
    uint32_t type;
    int32_t refcount;
};

struct gexgpu_instance
{
    struct gexgpu_object o;
};

struct gexgpu_adapter
{
    struct gexgpu_object o;
};

struct gexgpu_device
{
    struct gexgpu_object o;
    struct gexgpu_object *queue;
    struct gexgpu_object *adapter;
    uint32_t lost_cookie;
};

struct gexgpu_buffer
{
    struct gexgpu_object o;
    uint64_t size;
    WGPUBufferUsage usage;
    WGPUBufferMapState state;
    /* The mapped range's data, at map_offset in the buffer. */
    uint8_t *shadow;
    uint64_t map_offset, map_size;
    bool map_write;
};

struct gexgpu_texture
{
    struct gexgpu_object o;
    WGPUExtent3D size;
    uint32_t mip_level_count, sample_count;
    WGPUTextureDimension dimension;
    WGPUTextureFormat format;
    WGPUTextureUsage usage;
    WGPUTextureViewDimension binding_view_dimension;
};

struct gexgpu_query_set
{
    struct gexgpu_object o;
    WGPUQueryType type;
    uint32_t count;
};

enum gexgpu_callback_kind
{
    GEXGPU_CALLBACK_REQUEST_ADAPTER = 1,
    GEXGPU_CALLBACK_REQUEST_DEVICE,
    GEXGPU_CALLBACK_MAP,
    GEXGPU_CALLBACK_WORK_DONE,
    GEXGPU_CALLBACK_COMPILATION_INFO,
    GEXGPU_CALLBACK_POP_ERROR_SCOPE,
    GEXGPU_CALLBACK_CREATE_COMPUTE_PIPELINE,
    GEXGPU_CALLBACK_CREATE_RENDER_PIPELINE,
    GEXGPU_CALLBACK_DEVICE_LOST,
    GEXGPU_CALLBACK_UNCAPTURED_ERROR,
};

/* A callback the server will answer; its cookie is the future's id. */
struct gexgpu_callback
{
    enum gexgpu_callback_kind kind;
    WGPUCallbackMode mode;
    void (*callback)(void);
    void *userdata1, *userdata2;
    /* The object the callback creates or concerns. */
    struct gexgpu_object *object;
    /* The event's payload, once it has come; then whether it has fired. */
    uint8_t *event;
    size_t event_size;
    bool completed, fired;
    /* For uncaptured errors, which repeat: events not yet fired. */
    struct gexgpu_pending_event *pending;
};

struct gexgpu_pending_event
{
    struct gexgpu_pending_event *next;
    size_t size;
    uint8_t data[];
};

static struct
{
    volatile int lock;
    bool transport_ready;
    uint32_t next_id;
    struct gexgpu_writer commands;
    size_t command_start;
    /* The reply to the call the last message ended with. */
    struct gexgpu_reader reply;
    struct gexgpu_callback *callbacks;
    uint32_t callback_count, callback_capacity;
} client = {.next_id = 1};

static void client_lock(void)
{
    while (__atomic_exchange_n(&client.lock, 1, __ATOMIC_ACQUIRE))
        ;
}

static void client_unlock(void)
{
    __atomic_store_n(&client.lock, 0, __ATOMIC_RELEASE);
}

static void *client_alloc(struct gexgpu_reader *r, size_t size)
{
    return malloc(size);
}

void *gexgpu_client_alloc_out(size_t size)
{
    return malloc(size ? size : 1);
}

void gexgpu_client_unsupported_chain(WGPUSType stype)
{
    /* An extension the wire does not carry: the call goes without it. */
}

static uint32_t object_id(const void *object)
{
    return object ? ((const struct gexgpu_object *)object)->id : 0;
}

static inline void gexgpu_w_object(struct gexgpu_writer *w, const void *object)
{
    gexgpu_w_u32(w, object_id(object));
}

static size_t object_size(uint32_t type)
{
    switch (type)
    {
        case GEXGPU_TYPE_Device:
            return sizeof(struct gexgpu_device);
        case GEXGPU_TYPE_Buffer:
            return sizeof(struct gexgpu_buffer);
        case GEXGPU_TYPE_Texture:
            return sizeof(struct gexgpu_texture);
        case GEXGPU_TYPE_QuerySet:
            return sizeof(struct gexgpu_query_set);
        default:
            return sizeof(struct gexgpu_object);
    }
}

struct gexgpu_object *gexgpu_client_new_object(uint32_t type, struct gexgpu_object *parent)
{
    struct gexgpu_object *o;

    if (!(o = calloc(1, object_size(type))))
        abort();
    o->type = type;
    o->refcount = 1;
    o->id = __atomic_fetch_add(&client.next_id, 1, __ATOMIC_RELAXED);
    return o;
}

/* ---- messages ------------------------------------------------------------------------- */

static void process_reply(const uint8_t *data, size_t size);

/* Sends the batch; the reply's events are recorded, its call result kept
 * in client.reply. Called with the lock held. */
static void flush(void)
{
    const void *reply;
    size_t reply_size;

    if (!client.transport_ready)
    {
        if (!gexgpu_client_transport)
            gexgpu_client_init_transport();
        client.transport_ready = true;
    }
    if (!client.commands.size)
        return;
    reply = gexgpu_client_transport(gexgpu_client_transport_context,
            client.commands.data, client.commands.size, &reply_size);
    client.commands.size = 0;
    process_reply(reply, reply_size);
}

struct gexgpu_writer *gexgpu_client_begin(struct gexgpu_object *self, uint32_t op)
{
    client_lock();
    client.command_start = client.commands.size;
    gexgpu_w_u32(&client.commands, op);
    gexgpu_w_u32(&client.commands, 0);
    return &client.commands;
}

static void dispatch_ready(void);

void gexgpu_client_end(struct gexgpu_writer *w)
{
    gexgpu_w_patch_length(w, client.command_start);
    if (w->size >= GEXGPU_FLUSH_SIZE)
        flush();
    client_unlock();
    dispatch_ready();
}

/* Ends a command that waits for its result: the reader is the reply's,
 * the lock held until gexgpu_client_call_done(). */
struct gexgpu_reader *gexgpu_client_end_call(struct gexgpu_writer *w)
{
    gexgpu_w_patch_length(w, client.command_start);
    memset(&client.reply, 0, sizeof(client.reply));
    client.reply.error = true;
    flush();
    client.reply.alloc = client_alloc;
    return &client.reply;
}

void gexgpu_client_call_done(struct gexgpu_reader *r)
{
    client_unlock();
    dispatch_ready();
}

/* ---- callbacks ------------------------------------------------------------------------ */

/* Registers a callback; its cookie is also the future's id. Called with
 * the lock held. */
static uint32_t register_callback(enum gexgpu_callback_kind kind, WGPUCallbackMode mode, void (*callback)(void),
        void *userdata1, void *userdata2, struct gexgpu_object *object)
{
    struct gexgpu_callback *c;

    if (client.callback_count + 1 >= client.callback_capacity)
    {
        uint32_t capacity = client.callback_capacity ? client.callback_capacity * 2 : 64;
        struct gexgpu_callback *callbacks;

        if (!(callbacks = realloc(client.callbacks, capacity * sizeof(*callbacks))))
            abort();
        client.callbacks = callbacks;
        client.callback_capacity = capacity;
    }
    /* Cookie 0 is "none"; cookies index the table from 1. */
    if (!client.callback_count)
        client.callback_count = 1;
    c = &client.callbacks[client.callback_count];
    memset(c, 0, sizeof(*c));
    c->kind = kind;
    c->mode = mode;
    c->callback = callback;
    c->userdata1 = userdata1;
    c->userdata2 = userdata2;
    c->object = object;
    return client.callback_count++;
}

static struct gexgpu_callback *find_callback(uint32_t cookie)
{
    if (!cookie || cookie >= client.callback_count)
        return NULL;
    return &client.callbacks[cookie];
}

static void process_reply(const uint8_t *data, size_t size)
{
    struct gexgpu_reader r = {.data = data, .size = size, .alloc = client_alloc};

    while (!r.error && r.pos < r.size)
    {
        uint32_t kind = gexgpu_r_u32(&r);
        uint32_t length = gexgpu_r_u32(&r);
        size_t start = r.pos;
        struct gexgpu_callback *c;
        uint32_t cookie;

        if (r.error || length < 8 || length - 8 > r.size - start)
            break;
        switch (kind)
        {
            case GEXGPU_RECORD_REPLY:
                client.reply.data = data + start;
                client.reply.size = length - 8;
                client.reply.pos = 0;
                client.reply.error = false;
                break;

            case GEXGPU_RECORD_EVENT:
                cookie = gexgpu_r_u32(&r);
                if (!(c = find_callback(cookie)))
                    break;
                if (c->kind == GEXGPU_CALLBACK_UNCAPTURED_ERROR)
                {
                    struct gexgpu_pending_event *e, **tail;

                    if (!(e = malloc(sizeof(*e) + length - 12)))
                        break;
                    e->next = NULL;
                    e->size = length - 12;
                    memcpy(e->data, data + start + 4, e->size);
                    for (tail = &c->pending; *tail; tail = &(*tail)->next)
                        ;
                    *tail = e;
                    break;
                }
                free(c->event);
                if ((c->event = malloc(length - 12 ? length - 12 : 1)))
                    memcpy(c->event, data + start + 4, length - 12);
                c->event_size = length - 12;
                c->completed = true;
                break;
        }
        r.pos = start + length - 8;
    }
}

static void fire(struct gexgpu_callback *c, const uint8_t *event, size_t event_size);

/* Fires what may fire at any time: spontaneous callbacks and uncaptured
 * errors. Called without the lock. */
static void dispatch_ready(void)
{
    uint32_t i;

    for (i = 1; i < client.callback_count; ++i)
    {
        struct gexgpu_callback *c = &client.callbacks[i];

        if (c->kind == GEXGPU_CALLBACK_UNCAPTURED_ERROR)
        {
            while (c->pending)
            {
                struct gexgpu_pending_event *e = c->pending;

                c->pending = e->next;
                fire(c, e->data, e->size);
                free(e);
            }
            continue;
        }
        if (c->completed && !c->fired && c->mode == WGPUCallbackMode_AllowSpontaneous)
        {
            c->fired = true;
            fire(c, c->event, c->event_size);
        }
    }
}

static void release_object(struct gexgpu_object *o);

static void fire(struct gexgpu_callback *c, const uint8_t *event, size_t event_size)
{
    struct gexgpu_reader r = {.data = event, .size = event_size, .alloc = client_alloc};
    uint32_t status, type;
    WGPUStringView message;

    switch (c->kind)
    {
        case GEXGPU_CALLBACK_REQUEST_ADAPTER:
        {
            WGPUAdapter adapter = (WGPUAdapter)c->object;

            status = gexgpu_r_u32(&r);
            message = gexgpu_r_string(&r);
            if (status != WGPURequestAdapterStatus_Success)
            {
                release_object(c->object);
                adapter = NULL;
            }
            if (c->callback)
                ((WGPURequestAdapterCallback)c->callback)(status, adapter, message, c->userdata1, c->userdata2);
            free((void *)message.data);
            break;
        }

        case GEXGPU_CALLBACK_REQUEST_DEVICE:
        {
            WGPUDevice device = (WGPUDevice)c->object;

            status = gexgpu_r_u32(&r);
            message = gexgpu_r_string(&r);
            if (status != WGPURequestDeviceStatus_Success)
            {
                release_object(c->object);
                device = NULL;
            }
            if (c->callback)
                ((WGPURequestDeviceCallback)c->callback)(status, device, message, c->userdata1, c->userdata2);
            free((void *)message.data);
            break;
        }

        case GEXGPU_CALLBACK_MAP:
        {
            struct gexgpu_buffer *buffer = (struct gexgpu_buffer *)c->object;
            void *data;
            size_t size;

            status = gexgpu_r_u32(&r);
            message = gexgpu_r_string(&r);
            data = gexgpu_r_blob(&r, &size);
            if (status == WGPUMapAsyncStatus_Success && buffer->state == WGPUBufferMapState_Pending)
            {
                free(buffer->shadow);
                buffer->shadow = data;
                buffer->state = WGPUBufferMapState_Mapped;
                data = NULL;
            }
            else if (buffer->state == WGPUBufferMapState_Pending)
            {
                buffer->state = WGPUBufferMapState_Unmapped;
            }
            free(data);
            if (c->callback)
                ((WGPUBufferMapCallback)c->callback)(status, message, c->userdata1, c->userdata2);
            free((void *)message.data);
            release_object(&buffer->o);
            break;
        }

        case GEXGPU_CALLBACK_WORK_DONE:
            status = gexgpu_r_u32(&r);
            message = gexgpu_r_string(&r);
            if (c->callback)
                ((WGPUQueueWorkDoneCallback)c->callback)(status, message, c->userdata1, c->userdata2);
            free((void *)message.data);
            break;

        case GEXGPU_CALLBACK_COMPILATION_INFO:
        {
            WGPUCompilationInfo info = WGPU_COMPILATION_INFO_INIT;
            WGPUCompilationMessage *messages;
            uint32_t i, count;

            status = gexgpu_r_u32(&r);
            count = gexgpu_r_u32(&r);
            if (!(messages = calloc(count ? count : 1, sizeof(*messages))))
                break;
            for (i = 0; i < count && !r.error; ++i)
            {
                messages[i] = (WGPUCompilationMessage)WGPU_COMPILATION_MESSAGE_INIT;
                messages[i].message = gexgpu_r_string(&r);
                messages[i].type = gexgpu_r_u32(&r);
                messages[i].lineNum = gexgpu_r_u64(&r);
                messages[i].linePos = gexgpu_r_u64(&r);
                messages[i].offset = gexgpu_r_u64(&r);
                messages[i].length = gexgpu_r_u64(&r);
            }
            info.messageCount = i;
            info.messages = messages;
            if (c->callback)
                ((WGPUCompilationInfoCallback)c->callback)(status, &info, c->userdata1, c->userdata2);
            while (i--)
                free((void *)messages[i].message.data);
            free(messages);
            break;
        }

        case GEXGPU_CALLBACK_POP_ERROR_SCOPE:
            status = gexgpu_r_u32(&r);
            type = gexgpu_r_u32(&r);
            message = gexgpu_r_string(&r);
            if (c->callback)
                ((WGPUPopErrorScopeCallback)c->callback)(status, type, message, c->userdata1, c->userdata2);
            free((void *)message.data);
            break;

        case GEXGPU_CALLBACK_CREATE_COMPUTE_PIPELINE:
        case GEXGPU_CALLBACK_CREATE_RENDER_PIPELINE:
        {
            void *pipeline = c->object;

            status = gexgpu_r_u32(&r);
            message = gexgpu_r_string(&r);
            if (status != WGPUCreatePipelineAsyncStatus_Success)
            {
                release_object(c->object);
                pipeline = NULL;
            }
            if (c->callback && c->kind == GEXGPU_CALLBACK_CREATE_COMPUTE_PIPELINE)
                ((WGPUCreateComputePipelineAsyncCallback)c->callback)(status, pipeline, message,
                        c->userdata1, c->userdata2);
            else if (c->callback)
                ((WGPUCreateRenderPipelineAsyncCallback)c->callback)(status, pipeline, message,
                        c->userdata1, c->userdata2);
            free((void *)message.data);
            break;
        }

        case GEXGPU_CALLBACK_DEVICE_LOST:
        {
            WGPUDevice device = (WGPUDevice)c->object;

            status = gexgpu_r_u32(&r);
            message = gexgpu_r_string(&r);
            if (c->callback)
                ((WGPUDeviceLostCallback)c->callback)(&device, status, message, c->userdata1, c->userdata2);
            free((void *)message.data);
            break;
        }

        case GEXGPU_CALLBACK_UNCAPTURED_ERROR:
        {
            WGPUDevice device = (WGPUDevice)c->object;

            type = gexgpu_r_u32(&r);
            message = gexgpu_r_string(&r);
            if (c->callback)
                ((WGPUUncapturedErrorCallback)c->callback)(&device, type, message, c->userdata1, c->userdata2);
            free((void *)message.data);
            break;
        }
    }
}

/* Fires a completed callback that has not, if its mode allows it now. */
static bool fire_if(struct gexgpu_callback *c, bool waiting)
{
    if (!c->completed || c->fired)
        return false;
    if (!waiting && c->mode == WGPUCallbackMode_WaitAnyOnly)
        return false;
    c->fired = true;
    fire(c, c->event, c->event_size);
    return true;
}

static WGPUFuture future(uint32_t cookie)
{
    WGPUFuture f = {cookie};

    return f;
}

/* ---- objects -------------------------------------------------------------------------- */

void gexgpu_client_addref(struct gexgpu_object *o)
{
    if (o)
        __atomic_fetch_add(&o->refcount, 1, __ATOMIC_RELAXED);
}

static void release_object(struct gexgpu_object *o)
{
    struct gexgpu_writer *w;

    if (!o || __atomic_sub_fetch(&o->refcount, 1, __ATOMIC_ACQ_REL))
        return;

    w = gexgpu_client_begin(o, GEXGPU_OP_RELEASE);
    gexgpu_w_u32(w, o->id);
    gexgpu_client_end(w);

    switch (o->type)
    {
        case GEXGPU_TYPE_Buffer:
            free(((struct gexgpu_buffer *)o)->shadow);
            break;
        case GEXGPU_TYPE_Device:
            release_object(((struct gexgpu_device *)o)->queue);
            release_object(((struct gexgpu_device *)o)->adapter);
            break;
    }
    free(o);
}

void gexgpu_client_release(struct gexgpu_object *o)
{
    release_object(o);
}

#include "gen/wire-client-gen.c"

/* ---- instance ------------------------------------------------------------------------- */

WGPU_EXPORT WGPUInstance GEXGPU_CLIENT_NAME(wgpuCreateInstance)(WGPUInstanceDescriptor const *descriptor)
{
    struct gexgpu_object *instance = gexgpu_client_new_object(GEXGPU_TYPE_Instance, NULL);
    struct gexgpu_writer *w;

    w = gexgpu_client_begin(NULL, GEXGPU_OP_CreateInstance);
    gexgpu_w_u32(w, instance->id);
    gexgpu_client_end(w);
    return (WGPUInstance)instance;
}

WGPU_EXPORT WGPUProc GEXGPU_CLIENT_NAME(wgpuGetProcAddress)(WGPUStringView procName)
{
    return NULL;
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuGetInstanceFeatures)(WGPUSupportedInstanceFeatures *features)
{
    WGPUInstanceFeatureName *names = malloc(sizeof(*names));

    names[0] = WGPUInstanceFeatureName_TimedWaitAny;
    features->featureCount = 1;
    features->features = names;
}

WGPU_EXPORT WGPUBool GEXGPU_CLIENT_NAME(wgpuHasInstanceFeature)(WGPUInstanceFeatureName feature)
{
    return feature == WGPUInstanceFeatureName_TimedWaitAny;
}

WGPU_EXPORT WGPUStatus GEXGPU_CLIENT_NAME(wgpuGetInstanceLimits)(WGPUInstanceLimits *limits)
{
    limits->timedWaitAnyMaxCount = 64;
    return WGPUStatus_Success;
}

WGPU_EXPORT WGPUFuture GEXGPU_CLIENT_NAME(wgpuInstanceRequestAdapter)(WGPUInstance self,
        WGPURequestAdapterOptions const *options, WGPURequestAdapterCallbackInfo callbackInfo)
{
    struct gexgpu_object *adapter = gexgpu_client_new_object(GEXGPU_TYPE_Adapter, NULL);
    struct gexgpu_writer *w;
    uint32_t cookie;

    w = gexgpu_client_begin(&adapter[0], GEXGPU_OP_InstanceRequestAdapter);
    cookie = register_callback(GEXGPU_CALLBACK_REQUEST_ADAPTER, callbackInfo.mode,
            (void (*)(void))callbackInfo.callback, callbackInfo.userdata1, callbackInfo.userdata2, adapter);
    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, options != NULL);
    if (options)
        ser_WGPURequestAdapterOptions(w, options);
    gexgpu_w_u32(w, cookie);
    gexgpu_w_u32(w, adapter->id);
    gexgpu_client_end(w);
    return future(cookie);
}

WGPU_EXPORT WGPUWaitStatus GEXGPU_CLIENT_NAME(wgpuInstanceWaitAny)(WGPUInstance self, size_t futureCount,
        WGPUFutureWaitInfo *futures, uint64_t timeoutNS)
{
    struct gexgpu_writer *w;
    struct gexgpu_reader *r;
    bool any = false;
    uint32_t n = 0;
    size_t i;

    if (!futureCount)
        return WGPUWaitStatus_Success;

    /* Wait on the server only for futures with no event yet. */
    w = gexgpu_client_begin(NULL, GEXGPU_OP_WAIT);
    for (i = 0; i < futureCount; ++i)
    {
        struct gexgpu_callback *c = find_callback((uint32_t)futures[i].future.id);

        if (c && c->completed)
            any = true;
    }
    gexgpu_w_u64(w, any ? 0 : timeoutNS);
    for (i = 0; i < futureCount; ++i)
    {
        struct gexgpu_callback *c = find_callback((uint32_t)futures[i].future.id);

        if (c && !c->completed)
            ++n;
    }
    gexgpu_w_u32(w, n);
    for (i = 0; i < futureCount; ++i)
    {
        struct gexgpu_callback *c = find_callback((uint32_t)futures[i].future.id);

        if (c && !c->completed)
            gexgpu_w_u32(w, (uint32_t)futures[i].future.id);
    }
    r = gexgpu_client_end_call(w);
    gexgpu_client_call_done(r);

    for (i = 0; i < futureCount; ++i)
    {
        struct gexgpu_callback *c = find_callback((uint32_t)futures[i].future.id);

        if (!c)
            continue;
        fire_if(c, true);
        if ((futures[i].completed = c->fired))
            any = true;
    }
    return any ? WGPUWaitStatus_Success : WGPUWaitStatus_TimedOut;
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuInstanceProcessEvents)(WGPUInstance self)
{
    struct gexgpu_writer *w;
    struct gexgpu_reader *r;
    uint32_t i;

    w = gexgpu_client_begin(NULL, GEXGPU_OP_PROCESS_EVENTS);
    r = gexgpu_client_end_call(w);
    gexgpu_client_call_done(r);

    for (i = 1; i < client.callback_count; ++i)
        fire_if(&client.callbacks[i], false);
}

WGPU_EXPORT WGPUSurface GEXGPU_CLIENT_NAME(wgpuInstanceCreateSurface)(WGPUInstance self,
        WGPUSurfaceDescriptor const *descriptor)
{
    /* Presentation is the compositor's, not the wire's. */
    return NULL;
}

/* ---- adapter and device --------------------------------------------------------------- */

WGPU_EXPORT WGPUFuture GEXGPU_CLIENT_NAME(wgpuAdapterRequestDevice)(WGPUAdapter self,
        WGPUDeviceDescriptor const *descriptor, WGPURequestDeviceCallbackInfo callbackInfo)
{
    struct gexgpu_device *device = (struct gexgpu_device *)gexgpu_client_new_object(GEXGPU_TYPE_Device, NULL);
    uint32_t cookie, lost_cookie = 0, error_cookie = 0;
    struct gexgpu_writer *w;

    device->queue = gexgpu_client_new_object(GEXGPU_TYPE_Queue, NULL);
    device->adapter = (struct gexgpu_object *)self;
    gexgpu_client_addref(device->adapter);

    w = gexgpu_client_begin(&device->o, GEXGPU_OP_AdapterRequestDevice);
    cookie = register_callback(GEXGPU_CALLBACK_REQUEST_DEVICE, callbackInfo.mode,
            (void (*)(void))callbackInfo.callback, callbackInfo.userdata1, callbackInfo.userdata2, &device->o);
    /* The device lost future exists whether or not there is a callback. */
    lost_cookie = register_callback(GEXGPU_CALLBACK_DEVICE_LOST,
            descriptor && descriptor->deviceLostCallbackInfo.callback
            ? descriptor->deviceLostCallbackInfo.mode : WGPUCallbackMode_AllowProcessEvents,
            descriptor ? (void (*)(void))descriptor->deviceLostCallbackInfo.callback : NULL,
            descriptor ? descriptor->deviceLostCallbackInfo.userdata1 : NULL,
            descriptor ? descriptor->deviceLostCallbackInfo.userdata2 : NULL, &device->o);
    if (descriptor && descriptor->uncapturedErrorCallbackInfo.callback)
        error_cookie = register_callback(GEXGPU_CALLBACK_UNCAPTURED_ERROR, WGPUCallbackMode_AllowSpontaneous,
                (void (*)(void))descriptor->uncapturedErrorCallbackInfo.callback,
                descriptor->uncapturedErrorCallbackInfo.userdata1,
                descriptor->uncapturedErrorCallbackInfo.userdata2, &device->o);
    device->lost_cookie = lost_cookie;

    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, descriptor != NULL);
    if (descriptor)
        ser_WGPUDeviceDescriptor(w, descriptor);
    gexgpu_w_u32(w, cookie);
    gexgpu_w_u32(w, device->o.id);
    gexgpu_w_u32(w, device->queue->id);
    gexgpu_w_u32(w, lost_cookie);
    gexgpu_w_u32(w, error_cookie);
    gexgpu_client_end(w);
    return future(cookie);
}

WGPU_EXPORT WGPUQueue GEXGPU_CLIENT_NAME(wgpuDeviceGetQueue)(WGPUDevice self)
{
    struct gexgpu_device *device = (struct gexgpu_device *)self;

    gexgpu_client_addref(device->queue);
    return (WGPUQueue)device->queue;
}

WGPU_EXPORT WGPUFuture GEXGPU_CLIENT_NAME(wgpuDeviceGetLostFuture)(WGPUDevice self)
{
    return future(((struct gexgpu_device *)self)->lost_cookie);
}

WGPU_EXPORT WGPUAdapter GEXGPU_CLIENT_NAME(wgpuDeviceGetAdapter)(WGPUDevice self)
{
    struct gexgpu_device *device = (struct gexgpu_device *)self;

    gexgpu_client_addref(device->adapter);
    return (WGPUAdapter)device->adapter;
}

WGPU_EXPORT WGPUBuffer GEXGPU_CLIENT_NAME(wgpuDeviceCreateBuffer)(WGPUDevice self,
        WGPUBufferDescriptor const *descriptor)
{
    struct gexgpu_buffer *buffer = (struct gexgpu_buffer *)gexgpu_client_new_object(GEXGPU_TYPE_Buffer, NULL);
    struct gexgpu_writer *w;

    buffer->size = descriptor->size;
    buffer->usage = descriptor->usage;
    buffer->state = WGPUBufferMapState_Unmapped;
    if (descriptor->mappedAtCreation)
    {
        if (!(buffer->shadow = calloc(1, buffer->size ? buffer->size : 1)))
            abort();
        buffer->map_offset = 0;
        buffer->map_size = buffer->size;
        buffer->map_write = true;
        buffer->state = WGPUBufferMapState_Mapped;
    }

    w = gexgpu_client_begin(&buffer->o, GEXGPU_OP_DeviceCreateBuffer);
    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, 1);
    ser_WGPUBufferDescriptor(w, descriptor);
    gexgpu_w_u32(w, buffer->o.id);
    gexgpu_client_end(w);
    return (WGPUBuffer)buffer;
}

WGPU_EXPORT WGPUTexture GEXGPU_CLIENT_NAME(wgpuDeviceCreateTexture)(WGPUDevice self,
        WGPUTextureDescriptor const *descriptor)
{
    struct gexgpu_texture *texture = (struct gexgpu_texture *)gexgpu_client_new_object(GEXGPU_TYPE_Texture, NULL);
    struct gexgpu_writer *w;

    texture->size = descriptor->size;
    texture->mip_level_count = descriptor->mipLevelCount;
    texture->sample_count = descriptor->sampleCount;
    texture->dimension = descriptor->dimension;
    texture->format = descriptor->format;
    texture->usage = descriptor->usage;
    texture->binding_view_dimension = WGPUTextureViewDimension_Undefined;

    w = gexgpu_client_begin(&texture->o, GEXGPU_OP_DeviceCreateTexture);
    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, 1);
    ser_WGPUTextureDescriptor(w, descriptor);
    gexgpu_w_u32(w, texture->o.id);
    gexgpu_client_end(w);
    return (WGPUTexture)texture;
}

WGPU_EXPORT WGPUQuerySet GEXGPU_CLIENT_NAME(wgpuDeviceCreateQuerySet)(WGPUDevice self,
        WGPUQuerySetDescriptor const *descriptor)
{
    struct gexgpu_query_set *query_set = (struct gexgpu_query_set *)gexgpu_client_new_object(
            GEXGPU_TYPE_QuerySet, NULL);
    struct gexgpu_writer *w;

    query_set->type = descriptor->type;
    query_set->count = descriptor->count;

    w = gexgpu_client_begin(&query_set->o, GEXGPU_OP_DeviceCreateQuerySet);
    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, 1);
    ser_WGPUQuerySetDescriptor(w, descriptor);
    gexgpu_w_u32(w, query_set->o.id);
    gexgpu_client_end(w);
    return (WGPUQuerySet)query_set;
}

static WGPUFuture create_pipeline_async(WGPUDevice self, uint32_t op, enum gexgpu_callback_kind kind,
        uint32_t type, const void *descriptor, WGPUCallbackMode mode, void (*callback)(void),
        void *userdata1, void *userdata2)
{
    struct gexgpu_object *pipeline = gexgpu_client_new_object(type, NULL);
    struct gexgpu_writer *w;
    uint32_t cookie;

    w = gexgpu_client_begin(pipeline, op);
    cookie = register_callback(kind, mode, callback, userdata1, userdata2, pipeline);
    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, 1);
    if (kind == GEXGPU_CALLBACK_CREATE_COMPUTE_PIPELINE)
        ser_WGPUComputePipelineDescriptor(w, descriptor);
    else
        ser_WGPURenderPipelineDescriptor(w, descriptor);
    gexgpu_w_u32(w, cookie);
    gexgpu_w_u32(w, pipeline->id);
    gexgpu_client_end(w);
    return future(cookie);
}

WGPU_EXPORT WGPUFuture GEXGPU_CLIENT_NAME(wgpuDeviceCreateComputePipelineAsync)(WGPUDevice self,
        WGPUComputePipelineDescriptor const *descriptor, WGPUCreateComputePipelineAsyncCallbackInfo callbackInfo)
{
    return create_pipeline_async(self, GEXGPU_OP_DeviceCreateComputePipelineAsync,
            GEXGPU_CALLBACK_CREATE_COMPUTE_PIPELINE, GEXGPU_TYPE_ComputePipeline, descriptor, callbackInfo.mode,
            (void (*)(void))callbackInfo.callback, callbackInfo.userdata1, callbackInfo.userdata2);
}

WGPU_EXPORT WGPUFuture GEXGPU_CLIENT_NAME(wgpuDeviceCreateRenderPipelineAsync)(WGPUDevice self,
        WGPURenderPipelineDescriptor const *descriptor, WGPUCreateRenderPipelineAsyncCallbackInfo callbackInfo)
{
    return create_pipeline_async(self, GEXGPU_OP_DeviceCreateRenderPipelineAsync,
            GEXGPU_CALLBACK_CREATE_RENDER_PIPELINE, GEXGPU_TYPE_RenderPipeline, descriptor, callbackInfo.mode,
            (void (*)(void))callbackInfo.callback, callbackInfo.userdata1, callbackInfo.userdata2);
}

WGPU_EXPORT WGPUFuture GEXGPU_CLIENT_NAME(wgpuDevicePopErrorScope)(WGPUDevice self,
        WGPUPopErrorScopeCallbackInfo callbackInfo)
{
    struct gexgpu_writer *w;
    uint32_t cookie;

    w = gexgpu_client_begin((struct gexgpu_object *)self, GEXGPU_OP_DevicePopErrorScope);
    cookie = register_callback(GEXGPU_CALLBACK_POP_ERROR_SCOPE, callbackInfo.mode,
            (void (*)(void))callbackInfo.callback, callbackInfo.userdata1, callbackInfo.userdata2, NULL);
    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, cookie);
    gexgpu_client_end(w);
    return future(cookie);
}

/* ---- buffers -------------------------------------------------------------------------- */

WGPU_EXPORT WGPUFuture GEXGPU_CLIENT_NAME(wgpuBufferMapAsync)(WGPUBuffer self, WGPUMapMode mode,
        size_t offset, size_t size, WGPUBufferMapCallbackInfo callbackInfo)
{
    struct gexgpu_buffer *buffer = (struct gexgpu_buffer *)self;
    struct gexgpu_writer *w;
    uint32_t cookie;

    if (size == WGPU_WHOLE_MAP_SIZE)
        size = offset < buffer->size ? buffer->size - offset : 0;

    w = gexgpu_client_begin(&buffer->o, GEXGPU_OP_BufferMapAsync);
    /* The callback holds a reference: it writes to the buffer. */
    gexgpu_client_addref(&buffer->o);
    cookie = register_callback(GEXGPU_CALLBACK_MAP, callbackInfo.mode, (void (*)(void))callbackInfo.callback,
            callbackInfo.userdata1, callbackInfo.userdata2, &buffer->o);
    if (buffer->state == WGPUBufferMapState_Unmapped)
    {
        buffer->state = WGPUBufferMapState_Pending;
        buffer->map_offset = offset;
        buffer->map_size = size;
        buffer->map_write = mode & WGPUMapMode_Write;
    }
    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, cookie);
    gexgpu_w_u64(w, mode);
    gexgpu_w_u64(w, offset);
    gexgpu_w_u64(w, size);
    gexgpu_client_end(w);
    return future(cookie);
}

static uint8_t *mapped_range(struct gexgpu_buffer *buffer, size_t offset, size_t size)
{
    if (buffer->state != WGPUBufferMapState_Mapped || !buffer->shadow)
        return NULL;
    if (size == WGPU_WHOLE_MAP_SIZE)
        size = buffer->map_offset + buffer->map_size > offset ? buffer->map_offset + buffer->map_size - offset : 0;
    if (offset < buffer->map_offset || offset + size > buffer->map_offset + buffer->map_size)
        return NULL;
    return buffer->shadow + (offset - buffer->map_offset);
}

WGPU_EXPORT void *GEXGPU_CLIENT_NAME(wgpuBufferGetMappedRange)(WGPUBuffer self, size_t offset, size_t size)
{
    return mapped_range((struct gexgpu_buffer *)self, offset, size);
}

WGPU_EXPORT void const *GEXGPU_CLIENT_NAME(wgpuBufferGetConstMappedRange)(WGPUBuffer self,
        size_t offset, size_t size)
{
    return mapped_range((struct gexgpu_buffer *)self, offset, size);
}

WGPU_EXPORT WGPUStatus GEXGPU_CLIENT_NAME(wgpuBufferReadMappedRange)(WGPUBuffer self, size_t offset,
        void *data, size_t size)
{
    uint8_t *p;

    if (!(p = mapped_range((struct gexgpu_buffer *)self, offset, size)))
        return WGPUStatus_Error;
    memcpy(data, p, size);
    return WGPUStatus_Success;
}

WGPU_EXPORT WGPUStatus GEXGPU_CLIENT_NAME(wgpuBufferWriteMappedRange)(WGPUBuffer self, size_t offset,
        void const *data, size_t size)
{
    uint8_t *p;

    if (!(p = mapped_range((struct gexgpu_buffer *)self, offset, size)))
        return WGPUStatus_Error;
    memcpy(p, data, size);
    return WGPUStatus_Success;
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuBufferUnmap)(WGPUBuffer self)
{
    struct gexgpu_buffer *buffer = (struct gexgpu_buffer *)self;
    struct gexgpu_writer *w;

    /* What was written goes to the server's mapping before it is unmapped. */
    if (buffer->state == WGPUBufferMapState_Mapped && buffer->map_write && buffer->shadow)
    {
        w = gexgpu_client_begin(&buffer->o, GEXGPU_OP_BUFFER_UPDATE);
        gexgpu_w_object(w, self);
        gexgpu_w_u64(w, buffer->map_offset);
        gexgpu_w_blob(w, buffer->shadow, buffer->map_size);
        gexgpu_client_end(w);
    }
    free(buffer->shadow);
    buffer->shadow = NULL;
    buffer->state = WGPUBufferMapState_Unmapped;

    w = gexgpu_client_begin(&buffer->o, GEXGPU_OP_BufferUnmap);
    gexgpu_w_object(w, self);
    gexgpu_client_end(w);
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuBufferDestroy)(WGPUBuffer self)
{
    struct gexgpu_buffer *buffer = (struct gexgpu_buffer *)self;
    struct gexgpu_writer *w;

    free(buffer->shadow);
    buffer->shadow = NULL;
    buffer->state = WGPUBufferMapState_Unmapped;

    w = gexgpu_client_begin(&buffer->o, GEXGPU_OP_BufferDestroy);
    gexgpu_w_object(w, self);
    gexgpu_client_end(w);
}

WGPU_EXPORT uint64_t GEXGPU_CLIENT_NAME(wgpuBufferGetSize)(WGPUBuffer self)
{
    return ((struct gexgpu_buffer *)self)->size;
}

WGPU_EXPORT WGPUBufferUsage GEXGPU_CLIENT_NAME(wgpuBufferGetUsage)(WGPUBuffer self)
{
    return ((struct gexgpu_buffer *)self)->usage;
}

WGPU_EXPORT WGPUBufferMapState GEXGPU_CLIENT_NAME(wgpuBufferGetMapState)(WGPUBuffer self)
{
    return ((struct gexgpu_buffer *)self)->state;
}

/* ---- queue, shader modules ------------------------------------------------------------ */

WGPU_EXPORT WGPUFuture GEXGPU_CLIENT_NAME(wgpuQueueOnSubmittedWorkDone)(WGPUQueue self,
        WGPUQueueWorkDoneCallbackInfo callbackInfo)
{
    struct gexgpu_writer *w;
    uint32_t cookie;

    w = gexgpu_client_begin((struct gexgpu_object *)self, GEXGPU_OP_QueueOnSubmittedWorkDone);
    cookie = register_callback(GEXGPU_CALLBACK_WORK_DONE, callbackInfo.mode, (void (*)(void))callbackInfo.callback,
            callbackInfo.userdata1, callbackInfo.userdata2, NULL);
    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, cookie);
    gexgpu_client_end(w);
    return future(cookie);
}

WGPU_EXPORT WGPUFuture GEXGPU_CLIENT_NAME(wgpuShaderModuleGetCompilationInfo)(WGPUShaderModule self,
        WGPUCompilationInfoCallbackInfo callbackInfo)
{
    struct gexgpu_writer *w;
    uint32_t cookie;

    w = gexgpu_client_begin((struct gexgpu_object *)self, GEXGPU_OP_ShaderModuleGetCompilationInfo);
    cookie = register_callback(GEXGPU_CALLBACK_COMPILATION_INFO, callbackInfo.mode,
            (void (*)(void))callbackInfo.callback, callbackInfo.userdata1, callbackInfo.userdata2, NULL);
    gexgpu_w_object(w, self);
    gexgpu_w_u32(w, cookie);
    gexgpu_client_end(w);
    return future(cookie);
}

/* ---- textures and query sets: what the client knows ------------------------------------ */

#define TEXTURE(self) ((struct gexgpu_texture *)(self))

WGPU_EXPORT uint32_t GEXGPU_CLIENT_NAME(wgpuTextureGetWidth)(WGPUTexture self)
{
    return TEXTURE(self)->size.width;
}

WGPU_EXPORT uint32_t GEXGPU_CLIENT_NAME(wgpuTextureGetHeight)(WGPUTexture self)
{
    return TEXTURE(self)->size.height;
}

WGPU_EXPORT uint32_t GEXGPU_CLIENT_NAME(wgpuTextureGetDepthOrArrayLayers)(WGPUTexture self)
{
    return TEXTURE(self)->size.depthOrArrayLayers;
}

WGPU_EXPORT uint32_t GEXGPU_CLIENT_NAME(wgpuTextureGetMipLevelCount)(WGPUTexture self)
{
    return TEXTURE(self)->mip_level_count;
}

WGPU_EXPORT uint32_t GEXGPU_CLIENT_NAME(wgpuTextureGetSampleCount)(WGPUTexture self)
{
    return TEXTURE(self)->sample_count;
}

WGPU_EXPORT WGPUTextureDimension GEXGPU_CLIENT_NAME(wgpuTextureGetDimension)(WGPUTexture self)
{
    return TEXTURE(self)->dimension;
}

WGPU_EXPORT WGPUTextureFormat GEXGPU_CLIENT_NAME(wgpuTextureGetFormat)(WGPUTexture self)
{
    return TEXTURE(self)->format;
}

WGPU_EXPORT WGPUTextureUsage GEXGPU_CLIENT_NAME(wgpuTextureGetUsage)(WGPUTexture self)
{
    return TEXTURE(self)->usage;
}

WGPU_EXPORT WGPUTextureViewDimension GEXGPU_CLIENT_NAME(wgpuTextureGetTextureBindingViewDimension)(WGPUTexture self)
{
    return TEXTURE(self)->binding_view_dimension;
}

WGPU_EXPORT WGPUQueryType GEXGPU_CLIENT_NAME(wgpuQuerySetGetType)(WGPUQuerySet self)
{
    return ((struct gexgpu_query_set *)self)->type;
}

WGPU_EXPORT uint32_t GEXGPU_CLIENT_NAME(wgpuQuerySetGetCount)(WGPUQuerySet self)
{
    return ((struct gexgpu_query_set *)self)->count;
}

/* ---- surfaces: not on the wire --------------------------------------------------------- */

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuSurfaceConfigure)(WGPUSurface self, WGPUSurfaceConfiguration const *config)
{
}

WGPU_EXPORT WGPUStatus GEXGPU_CLIENT_NAME(wgpuSurfaceGetCapabilities)(WGPUSurface self, WGPUAdapter adapter,
        WGPUSurfaceCapabilities *capabilities)
{
    return WGPUStatus_Error;
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuSurfaceGetCurrentTexture)(WGPUSurface self, WGPUSurfaceTexture *texture)
{
    texture->texture = NULL;
    texture->status = WGPUSurfaceGetCurrentTextureStatus_Error;
}

WGPU_EXPORT WGPUStatus GEXGPU_CLIENT_NAME(wgpuSurfacePresent)(WGPUSurface self)
{
    return WGPUStatus_Error;
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuSurfaceUnconfigure)(WGPUSurface self)
{
}

/* ---- results the caller frees ---------------------------------------------------------- */

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuAdapterInfoFreeMembers)(WGPUAdapterInfo value)
{
    free((void *)value.vendor.data);
    free((void *)value.architecture.data);
    free((void *)value.device.data);
    free((void *)value.description.data);
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuSupportedFeaturesFreeMembers)(WGPUSupportedFeatures value)
{
    free((void *)value.features);
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuSupportedWGSLLanguageFeaturesFreeMembers)(
        WGPUSupportedWGSLLanguageFeatures value)
{
    free((void *)value.features);
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuSupportedInstanceFeaturesFreeMembers)(WGPUSupportedInstanceFeatures value)
{
    free((void *)value.features);
}

WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpuSurfaceCapabilitiesFreeMembers)(WGPUSurfaceCapabilities value)
{
}
