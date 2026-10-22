#define COBJMACROS
#include <d3d11.h>
#include <windows.h>
static unsigned pixels[] = { 1, 2, 99, 3, 4, 99 };
static void *volatile data = pixels;
int entry(void) {
    PFN_D3D11_CREATE_DEVICE create = (void *)GetProcAddress(LoadLibraryA("d3d11.dll"), "D3D11CreateDevice");
    ID3D11Device *device = 0;
    ID3D11DeviceContext *context = 0;
    if (!create || FAILED(create(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0, 0, 0, 7, &device, 0, &context))) {
        return 1;
    }
    D3D11_TEXTURE2D_DESC desc = {
        2, 2, 1, 1, DXGI_FORMAT_R8G8B8A8_UNORM, { 1, 0 }, D3D11_USAGE_DEFAULT, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET, 0, 0
    };
    D3D11_SUBRESOURCE_DATA initial = { data, 12, 0 };
    ID3D11Texture2D *texture = 0, *staging = 0;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &desc, &initial, &texture))) {
        return 2;
    }
    D3D11_TEXTURE2D_DESC actual;
    ID3D11Texture2D_GetDesc(texture, &actual);
    if (actual.Width != 2 || actual.Height != 2 || actual.BindFlags != desc.BindFlags) {
        return 3;
    }
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &desc, 0, &staging))) {
        return 4;
    }
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)texture);
    D3D11_MAPPED_SUBRESOURCE mapped;
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        return 5;
    }
    unsigned *read = mapped.pData;
    if (mapped.RowPitch != 8 || mapped.DepthPitch != 16 || read[0] != 1 || read[1] != 2 || read[2] != 3 || read[3] != 4) {
        return 6;
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    unsigned replacement = 42;
    D3D11_BOX box = { 1, 1, 0, 2, 2, 1 };
    ID3D11DeviceContext_UpdateSubresource(context, (ID3D11Resource *)texture, 0, &box, &replacement, 4, 0);
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)texture);
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        return 7;
    }
    read = mapped.pData;
    if (read[0] != 1 || read[3] != 42) {
        return 8;
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    ID3D11RenderTargetView *rtv = 0;
    if (FAILED(ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)texture, 0, &rtv))) {
        return 9;
    }
    D3D11_RENDER_TARGET_VIEW_DESC explicit_desc = { 0 };
    explicit_desc.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2D;
    ID3D11RenderTargetView *explicit_view = 0;
    if (ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)texture, &explicit_desc, 0) != S_FALSE) {
        return 31;
    }
    if (FAILED(ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)texture, &explicit_desc, &explicit_view))) {
        return 32;
    }
    ID3D11RenderTargetView_Release(rtv);
    rtv = explicit_view;
    explicit_desc.Texture2D.MipSlice = 1;
    if (ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)texture, &explicit_desc, &explicit_view) != E_INVALIDARG ||
        explicit_view) {
        return 33;
    }
    D3D11_RENDER_TARGET_VIEW_DESC rtvdesc;
    ID3D11RenderTargetView_GetDesc(rtv, &rtvdesc);
    if (rtvdesc.Format != DXGI_FORMAT_R8G8B8A8_UNORM || rtvdesc.ViewDimension != D3D11_RTV_DIMENSION_TEXTURE2D ||
        rtvdesc.Texture2D.MipSlice) {
        return 23;
    }
    ID3D11Resource *view_resource = 0;
    ID3D11RenderTargetView_GetResource(rtv, &view_resource);
    if (view_resource != (ID3D11Resource *)texture) {
        return 24;
    }
    ID3D11Resource_Release(view_resource);
    float color[4] = { 1, 0, 0, 1 };
    ID3D11DeviceContext_ClearRenderTargetView(context, rtv, color);
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, &rtv, 0);
    ID3D11RenderTargetView *bound[2] = { 0 };
    ID3D11DeviceContext_OMGetRenderTargets(context, 2, bound, 0);
    if (bound[0] != rtv || bound[1]) {
        return 20;
    }
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, bound, 0);
    ID3D11RenderTargetView_Release(bound[0]);
    ID3D11DeviceContext_OMSetRenderTargets(context, 0, 0, 0);
    ID3D11DeviceContext_OMGetRenderTargets(context, 1, bound, 0);
    if (bound[0]) {
        return 21;
    }
    ID3D11ShaderResourceView *srv = 0, *queried = 0;
    if (FAILED(ID3D11Device_CreateShaderResourceView(device, (ID3D11Resource *)texture, 0, &srv))) {
        return 25;
    }
    ID3D11DeviceContext_PSSetShaderResources(context, 7, 1, &srv);
    ID3D11DeviceContext_VSSetShaderResources(context, 3, 1, &srv);
    ID3D11DeviceContext_PSGetShaderResources(context, 7, 1, &queried);
    if (queried != srv) {
        return 26;
    }
    ID3D11ShaderResourceView_Release(queried);
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, &rtv, 0);
    ID3D11DeviceContext_PSGetShaderResources(context, 7, 1, &queried);
    if (queried) {
        return 27;
    }
    ID3D11DeviceContext_VSGetShaderResources(context, 3, 1, &queried);
    if (queried) {
        return 28;
    }
    ID3D11DeviceContext_PSSetShaderResources(context, 7, 1, &srv);
    ID3D11DeviceContext_PSGetShaderResources(context, 7, 1, &queried);
    if (queried) {
        return 29;
    }
    ID3D11DeviceContext_OMSetRenderTargets(context, 0, 0, 0);
    ID3D11DeviceContext_PSSetShaderResources(context, 7, 1, &srv);
    ID3D11ShaderResourceView_Release(srv);
    ID3D11DeviceContext_PSGetShaderResources(context, 7, 1, &queried);
    if (queried != srv) {
        return 30;
    }
    ID3D11DeviceContext_PSSetShaderResources(context, 7, 1, &queried);
    ID3D11ShaderResourceView_Release(queried);
    ID3D11DeviceContext_PSSetShaderResources(context, 7, 1, 0);
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)texture);
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        return 10;
    }
    read = mapped.pData;
    if (read[0] != 0xff0000ff || read[3] != 0xff0000ff) {
        return 11;
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.CPUAccessFlags = 0;
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    initial.SysMemPitch = 4;
    ID3D11Texture2D *bad = 0;
    if (ID3D11Device_CreateTexture2D(device, &desc, &initial, &bad) != E_INVALIDARG || bad) {
        return 12;
    }
    desc.MipLevels = 2;
    if (ID3D11Device_CreateTexture2D(device, &desc, 0, &bad) != E_INVALIDARG || bad) {
        return 13;
    }
    ID3D11RenderTargetView_Release(rtv);
    ID3D11Texture2D_Release(texture);
    ID3D11Texture2D_Release(staging);
    desc.MipLevels = 1;
    desc.Format = DXGI_FORMAT_D16_UNORM;
    desc.BindFlags = D3D11_BIND_DEPTH_STENCIL;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &desc, 0, &texture))) {
        return 14;
    }
    ID3D11DepthStencilView *dsv = 0;
    if (FAILED(ID3D11Device_CreateDepthStencilView(device, (ID3D11Resource *)texture, 0, &dsv))) {
        return 15;
    }
    D3D11_DEPTH_STENCIL_VIEW_DESC viewdesc;
    ID3D11DepthStencilView_GetDesc(dsv, &viewdesc);
    if (viewdesc.Format != DXGI_FORMAT_D16_UNORM || viewdesc.ViewDimension != D3D11_DSV_DIMENSION_TEXTURE2D) {
        return 16;
    }
    ID3D11DeviceContext_ClearDepthStencilView(context, dsv, D3D11_CLEAR_DEPTH, 0.5f, 0);
    ID3D11DeviceContext_OMSetRenderTargets(context, 0, 0, dsv);
    ID3D11DepthStencilView *bound_depth = 0;
    ID3D11DepthStencilView_Release(dsv);
    ID3D11DeviceContext_OMGetRenderTargets(context, 0, 0, &bound_depth);
    if (bound_depth != dsv) {
        return 22;
    }
    ID3D11DeviceContext_OMSetRenderTargets(context, 0, 0, 0);
    dsv = bound_depth;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &desc, 0, &staging))) {
        return 17;
    }
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)texture);
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        return 18;
    }
    unsigned short *depth = mapped.pData;
    if (mapped.RowPitch != 4 || depth[0] != 32768 || depth[3] != 32768) {
        return 19;
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    ID3D11DepthStencilView_Release(dsv);
    ID3D11Texture2D_Release(texture);
    ID3D11Texture2D_Release(staging);
    ID3D11DeviceContext_Release(context);
    ID3D11Device_Release(device);
    return 0;
}
