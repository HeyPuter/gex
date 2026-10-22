#define COBJMACROS
#include <d3d11.h>
#include <windows.h>
static unsigned initial[4] = { 1, 2, 3, 4 };
static unsigned *volatile relocated = initial;
int entry(void) {
    PFN_D3D11_CREATE_DEVICE create = (void *)GetProcAddress(LoadLibraryA("d3d11.dll"), "D3D11CreateDevice");
    ID3D11Device *device = 0;
    ID3D11DeviceContext *context = 0;
    if (!create) {
        return 1;
    }
    if (create(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0x80000000u, 0, 0, 7, &device, 0, &context) != E_INVALIDARG || device || context) {
        return 12;
    }
    D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0 }, chosen = 0;
    if (FAILED(create(0, D3D_DRIVER_TYPE_HARDWARE, 0, D3D11_CREATE_DEVICE_SINGLETHREADED, levels, 2, 7, &device, &chosen, &context)) ||
        chosen != D3D_FEATURE_LEVEL_11_0) {
        return 1;
    }
    UINT support = 0, quality = 99;
    if (FAILED(ID3D11Device_CheckFormatSupport(device, DXGI_FORMAT_R8G8B8A8_UNORM, &support)) ||
        !(support & D3D11_FORMAT_SUPPORT_RENDER_TARGET) || (support & D3D11_FORMAT_SUPPORT_SHADER_SAMPLE)) {
        return 13;
    }
    if (FAILED(ID3D11Device_CheckFormatSupport(device, DXGI_FORMAT_D24_UNORM_S8_UINT, &support)) || support) {
        return 14;
    }
    if (FAILED(ID3D11Device_CheckMultisampleQualityLevels(device, DXGI_FORMAT_R8G8B8A8_UNORM, 1, &quality)) || quality != 1) {
        return 15;
    }
    if (FAILED(ID3D11Device_CheckMultisampleQualityLevels(device, DXGI_FORMAT_R8G8B8A8_UNORM, 4, &quality)) || quality) {
        return 16;
    }
    if (ID3D11Device_CheckMultisampleQualityLevels(device, DXGI_FORMAT_R8G8B8A8_UNORM, 0, &quality) != E_INVALIDARG || quality) {
        return 17;
    }
    D3D11_BUFFER_DESC desc = { 16, D3D11_USAGE_DYNAMIC, D3D11_BIND_VERTEX_BUFFER, D3D11_CPU_ACCESS_WRITE, 0, 0 };
    D3D11_SUBRESOURCE_DATA input = { relocated, 0, 0 };
    ID3D11Buffer *buffer = 0, *staging = 0;
    if (FAILED(ID3D11Device_CreateBuffer(device, &desc, &input, &buffer))) {
        return 2;
    }
    UINT stride = 4, offset = 4, got_stride = 0, got_offset = 0;
    ID3D11Buffer *bound = 0;
    ID3D11DeviceContext_IASetPrimitiveTopology(context, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    D3D11_PRIMITIVE_TOPOLOGY topology;
    ID3D11DeviceContext_IAGetPrimitiveTopology(context, &topology);
    if (topology != D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST) {
        return 18;
    }
    ID3D11DeviceContext_IASetVertexBuffers(context, 3, 1, &buffer, &stride, &offset);
    ID3D11DeviceContext_IAGetVertexBuffers(context, 3, 1, &bound, &got_stride, &got_offset);
    if (bound != buffer || got_stride != 4 || got_offset != 4) {
        return 19;
    }
    ID3D11Buffer_Release(bound);
    ID3D11DeviceContext_IASetVertexBuffers(context, 3, 1, 0, 0, 0);
    ID3D11DeviceContext_IAGetVertexBuffers(context, 3, 1, &bound, 0, 0);
    if (bound) {
        return 20;
    }
    D3D11_BUFFER_DESC idesc = { 16, D3D11_USAGE_DEFAULT, D3D11_BIND_INDEX_BUFFER, 0, 0, 0 };
    ID3D11Buffer *index = 0;
    if (FAILED(ID3D11Device_CreateBuffer(device, &idesc, 0, &index))) {
        return 21;
    }
    ID3D11DeviceContext_IASetIndexBuffer(context, index, DXGI_FORMAT_R16_UINT, 2);
    ID3D11Buffer_Release(index);
    DXGI_FORMAT format;
    ID3D11DeviceContext_IAGetIndexBuffer(context, &bound, &format, &got_offset);
    if (bound != index || format != DXGI_FORMAT_R16_UINT || got_offset != 2) {
        return 22;
    }
    ID3D11DeviceContext_IASetIndexBuffer(context, bound, format, got_offset);
    ID3D11Buffer_Release(bound);
    ID3D11DeviceContext_IASetIndexBuffer(context, 0, DXGI_FORMAT_UNKNOWN, 0);
    D3D11_BUFFER_DESC got;
    ID3D11Buffer_GetDesc(buffer, &got);
    if (got.ByteWidth != 16) {
        return 3;
    }
    D3D11_MAPPED_SUBRESOURCE mapped;
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, &mapped))) {
        return 4;
    }
    for (unsigned i = 0; i < 4; ++i) {
        ((unsigned *)mapped.pData)[i] = 10 + i;
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)buffer, 0);
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(ID3D11Device_CreateBuffer(device, &desc, 0, &staging))) {
        return 5;
    }
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)buffer);
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        return 6;
    }
    for (unsigned i = 0; i < 4; ++i) {
        if (((unsigned *)mapped.pData)[i] != 10 + i) {
            return 7;
        }
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    if (SUCCEEDED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_WRITE, 0, &mapped))) {
        return 8;
    }
    ID3D11Buffer *constant = 0;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    desc.CPUAccessFlags = 0;
    if (FAILED(ID3D11Device_CreateBuffer(device, &desc, 0, &constant))) {
        return 9;
    }
    ID3D11DeviceContext_VSSetConstantBuffers(context, 1, 1, &constant);
    ID3D11DeviceContext_PSSetConstantBuffers(context, 2, 1, &constant);
    ID3D11DeviceContext_VSGetConstantBuffers(context, 1, 1, &bound);
    if (bound != constant) {
        return 23;
    }
    ID3D11DeviceContext_VSSetConstantBuffers(context, 1, 1, &bound);
    ID3D11Buffer_Release(bound);
    ID3D11DeviceContext_PSGetConstantBuffers(context, 2, 1, &bound);
    if (bound != constant) {
        return 24;
    }
    ID3D11Buffer_Release(bound);
    ID3D11DeviceContext_VSSetConstantBuffers(context, 1, 1, 0);
    ID3D11DeviceContext_PSSetConstantBuffers(context, 2, 1, 0);
    ID3D11DeviceContext_UpdateSubresource(context, (ID3D11Resource *)constant, 0, 0, relocated, 0, 0);
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)constant);
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        return 10;
    }
    for (unsigned i = 0; i < 4; ++i) {
        if (((unsigned *)mapped.pData)[i] != i + 1) {
            return 11;
        }
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    ID3D11Buffer_Release(constant);
    ID3D11Buffer_Release(staging);
    ID3D11Buffer_Release(buffer);
    ID3D11DeviceContext_Release(context);
    ID3D11Device_Release(device);
    return 0;
}
