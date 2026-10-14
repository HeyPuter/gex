/* A guest's first WebGPU: adapter, device, a compute shader that doubles a
 * buffer, and the result read back, through the wire and /dev/gex-gpu.
 * Prints "gpu-smoke: ok" and exits 0 when the numbers come back doubled. */

#include <stdio.h>
#include <string.h>

#include "webgpu.h"

#define COUNT 1024

static WGPUAdapter adapter;
static WGPUDevice device;
static int mapped;

static WGPUStringView sv(const char *s)
{
    WGPUStringView v = {s, WGPU_STRLEN};
    return v;
}

static void on_adapter(WGPURequestAdapterStatus status, WGPUAdapter a, WGPUStringView message, void *u1, void *u2)
{
    if (status != WGPURequestAdapterStatus_Success)
        fprintf(stderr, "gpu-smoke: no adapter: %.*s\n", (int)message.length, message.data);
    adapter = a;
}

static void on_device(WGPURequestDeviceStatus status, WGPUDevice d, WGPUStringView message, void *u1, void *u2)
{
    if (status != WGPURequestDeviceStatus_Success)
        fprintf(stderr, "gpu-smoke: no device: %.*s\n", (int)message.length, message.data);
    device = d;
}

static void on_error(WGPUDevice const *d, WGPUErrorType type, WGPUStringView message, void *u1, void *u2)
{
    fprintf(stderr, "gpu-smoke: error %d: %.*s\n", type, (int)message.length, message.data);
}

static void on_map(WGPUMapAsyncStatus status, WGPUStringView message, void *u1, void *u2)
{
    mapped = status == WGPUMapAsyncStatus_Success ? 1 : -1;
}

static int wait(WGPUInstance instance, WGPUFuture future)
{
    WGPUFutureWaitInfo info = {future, 0};

    return wgpuInstanceWaitAny(instance, 1, &info, UINT64_MAX) == WGPUWaitStatus_Success;
}

int main(void)
{
    static const WGPUInstanceFeatureName features[] = {WGPUInstanceFeatureName_TimedWaitAny};
    WGPUInstanceDescriptor instance_desc = WGPU_INSTANCE_DESCRIPTOR_INIT;
    WGPURequestAdapterCallbackInfo adapter_cb = WGPU_REQUEST_ADAPTER_CALLBACK_INFO_INIT;
    WGPURequestDeviceCallbackInfo device_cb = WGPU_REQUEST_DEVICE_CALLBACK_INFO_INIT;
    WGPUDeviceDescriptor device_desc = WGPU_DEVICE_DESCRIPTOR_INIT;
    WGPUShaderSourceWGSL wgsl = WGPU_SHADER_SOURCE_WGSL_INIT;
    WGPUShaderModuleDescriptor module_desc = WGPU_SHADER_MODULE_DESCRIPTOR_INIT;
    WGPUComputePipelineDescriptor pipeline_desc = WGPU_COMPUTE_PIPELINE_DESCRIPTOR_INIT;
    WGPUBufferDescriptor buffer_desc = WGPU_BUFFER_DESCRIPTOR_INIT;
    WGPUBindGroupEntry entry = WGPU_BIND_GROUP_ENTRY_INIT;
    WGPUBindGroupDescriptor group_desc = WGPU_BIND_GROUP_DESCRIPTOR_INIT;
    WGPUBufferMapCallbackInfo map_cb = WGPU_BUFFER_MAP_CALLBACK_INFO_INIT;
    WGPUAdapterInfo adapter_info = WGPU_ADAPTER_INFO_INIT;
    WGPUInstance instance;
    WGPUShaderModule module;
    WGPUComputePipeline pipeline;
    WGPUBuffer storage, readback;
    WGPUBindGroup group;
    WGPUCommandEncoder encoder;
    WGPUComputePassEncoder pass;
    WGPUCommandBuffer commands;
    WGPUQueue queue;
    const uint32_t *result;
    uint32_t data[COUNT];
    unsigned int i;

    instance_desc.requiredFeatureCount = 1;
    instance_desc.requiredFeatures = features;
    if (!(instance = wgpuCreateInstance(&instance_desc)))
        return printf("gpu-smoke: no instance\n"), 1;

    adapter_cb.mode = WGPUCallbackMode_WaitAnyOnly;
    adapter_cb.callback = on_adapter;
    if (!wait(instance, wgpuInstanceRequestAdapter(instance, NULL, adapter_cb)) || !adapter)
        return printf("gpu-smoke: no adapter\n"), 1;
    wgpuAdapterGetInfo(adapter, &adapter_info);
    printf("gpu-smoke: adapter \"%.*s\" (%.*s)\n", (int)adapter_info.device.length, adapter_info.device.data,
            (int)adapter_info.description.length, adapter_info.description.data);

    device_desc.uncapturedErrorCallbackInfo.callback = on_error;
    device_cb.mode = WGPUCallbackMode_WaitAnyOnly;
    device_cb.callback = on_device;
    if (!wait(instance, wgpuAdapterRequestDevice(adapter, &device_desc, device_cb)) || !device)
        return printf("gpu-smoke: no device\n"), 1;
    queue = wgpuDeviceGetQueue(device);

    wgsl.code = sv("@group(0) @binding(0) var<storage, read_write> v: array<u32>;\n"
            "@compute @workgroup_size(64) fn main(@builtin(global_invocation_id) id: vec3u) {\n"
            "    v[id.x] = v[id.x] * 2u + 1u;\n"
            "}\n");
    module_desc.nextInChain = &wgsl.chain;
    module = wgpuDeviceCreateShaderModule(device, &module_desc);
    pipeline_desc.compute.module = module;
    pipeline_desc.compute.entryPoint = sv("main");
    pipeline = wgpuDeviceCreateComputePipeline(device, &pipeline_desc);

    buffer_desc.size = sizeof(data);
    buffer_desc.usage = WGPUBufferUsage_Storage | WGPUBufferUsage_CopySrc | WGPUBufferUsage_CopyDst;
    storage = wgpuDeviceCreateBuffer(device, &buffer_desc);
    buffer_desc.usage = WGPUBufferUsage_MapRead | WGPUBufferUsage_CopyDst;
    readback = wgpuDeviceCreateBuffer(device, &buffer_desc);
    for (i = 0; i < COUNT; ++i)
        data[i] = i;
    wgpuQueueWriteBuffer(queue, storage, 0, data, sizeof(data));

    entry.binding = 0;
    entry.buffer = storage;
    entry.size = sizeof(data);
    group_desc.layout = wgpuComputePipelineGetBindGroupLayout(pipeline, 0);
    group_desc.entryCount = 1;
    group_desc.entries = &entry;
    group = wgpuDeviceCreateBindGroup(device, &group_desc);

    encoder = wgpuDeviceCreateCommandEncoder(device, NULL);
    pass = wgpuCommandEncoderBeginComputePass(encoder, NULL);
    wgpuComputePassEncoderSetPipeline(pass, pipeline);
    wgpuComputePassEncoderSetBindGroup(pass, 0, group, 0, NULL);
    wgpuComputePassEncoderDispatchWorkgroups(pass, COUNT / 64, 1, 1);
    wgpuComputePassEncoderEnd(pass);
    wgpuCommandEncoderCopyBufferToBuffer(encoder, storage, 0, readback, 0, sizeof(data));
    commands = wgpuCommandEncoderFinish(encoder, NULL);
    wgpuQueueSubmit(queue, 1, &commands);

    map_cb.mode = WGPUCallbackMode_WaitAnyOnly;
    map_cb.callback = on_map;
    if (!wait(instance, wgpuBufferMapAsync(readback, WGPUMapMode_Read, 0, sizeof(data), map_cb)) || mapped != 1)
        return printf("gpu-smoke: map failed\n"), 1;
    result = wgpuBufferGetConstMappedRange(readback, 0, sizeof(data));
    for (i = 0; i < COUNT; ++i)
    {
        if (result[i] != i * 2 + 1)
            return printf("gpu-smoke: v[%u] = %u, not %u\n", i, result[i], i * 2 + 1), 1;
    }
    wgpuBufferUnmap(readback);
    printf("gpu-smoke: ok\n");
    return 0;
}
