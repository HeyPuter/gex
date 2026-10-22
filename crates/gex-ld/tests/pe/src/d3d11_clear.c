#define COBJMACROS
#include <d3d11.h>
#include <dxgi.h>
#include <windows.h>

static const GUID texture_iid = { 0x6f15aaf2, 0xd208, 0x4e89, { 0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c } };
static const GUID *volatile relocation = &texture_iid;
int entry(void) {
    HMODULE library = LoadLibraryA("d3d11.dll");
    PFN_D3D11_CREATE_DEVICE_AND_SWAP_CHAIN create = (void *)GetProcAddress(library, "D3D11CreateDeviceAndSwapChain");
    if (!create) {
        return 1;
    }
    DXGI_SWAP_CHAIN_DESC desc = { 0 };
    desc.BufferDesc.Width = 320;
    desc.BufferDesc.Height = 240;
    desc.BufferDesc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT | DXGI_USAGE_SHADER_INPUT;
    desc.BufferCount = 2;
    desc.Windowed = TRUE;
    desc.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;
    desc.Flags = DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH;
    ID3D11Device *device = 0;
    ID3D11DeviceContext *context = 0;
    IDXGISwapChain *swap = 0;
    HRESULT hr;
#ifdef USE_DXGI_FACTORY
    static const GUID factory_iid = { 0x770aae78, 0xf26f, 0x4dba, { 0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87 } };
    HRESULT (*factory_create)(const GUID *, void **) = (void *)GetProcAddress(LoadLibraryA("dxgi.dll"), "CreateDXGIFactory1");
    PFN_D3D11_CREATE_DEVICE device_create = (void *)GetProcAddress(library, "D3D11CreateDevice");
    IDXGIFactory1 *factory = 0;
    IDXGIAdapter1 *adapter = 0;
    IDXGIOutput *output = 0;
    if (!factory_create || FAILED(factory_create(&factory_iid, (void **)&factory))) {
        return 10;
    }
    IDXGIFactory1 *queried = 0;
    if (FAILED(IDXGIFactory1_QueryInterface(factory, &factory_iid, (void **)&queried)) || queried != factory) {
        return 18;
    }
    IDXGIFactory1_Release(queried);
    if (FAILED(IDXGIFactory1_EnumAdapters1(factory, 0, &adapter))) {
        return 11;
    }
    IDXGIAdapter1 *missing = 0;
    if (IDXGIFactory1_EnumAdapters1(factory, 1, &missing) != DXGI_ERROR_NOT_FOUND || missing) {
        return 12;
    }
    DXGI_ADAPTER_DESC1 adapter_desc;
    if (FAILED(IDXGIAdapter1_GetDesc1(adapter, &adapter_desc)) || !adapter_desc.Description[0]) {
        return 13;
    }
    if (FAILED(IDXGIAdapter1_EnumOutputs(adapter, 0, &output))) {
        return 14;
    }
    UINT count = 0;
    DXGI_MODE_DESC mode;
    if (FAILED(IDXGIOutput_GetDisplayModeList(output, desc.BufferDesc.Format, 0, &count, 0)) || count != 1) {
        return 15;
    }
    if (FAILED(IDXGIOutput_GetDisplayModeList(output, desc.BufferDesc.Format, 0, &count, &mode)) || mode.Width != 1280 ||
        mode.Height != 720) {
        return 16;
    }
    D3D_FEATURE_LEVEL unsupported_level = D3D_FEATURE_LEVEL_11_1;
    if (device_create(
            (IDXGIAdapter *)adapter, D3D_DRIVER_TYPE_UNKNOWN, 0, 0, &unsupported_level, 1, D3D11_SDK_VERSION, &device, 0, &context
        ) != DXGI_ERROR_UNSUPPORTED ||
        device || context) {
        return 19;
    }
    hr = device_create((IDXGIAdapter *)adapter, D3D_DRIVER_TYPE_UNKNOWN, 0, 0, 0, 0, D3D11_SDK_VERSION, &device, 0, &context);
    if (FAILED(hr)) {
        return 17;
    }
    desc.Flags = 0x80000000u;
    if (IDXGIFactory1_CreateSwapChain(factory, (IUnknown *)device, &desc, &swap) != E_INVALIDARG || swap) {
        return 28;
    }
    desc.Flags = DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH;
    hr = IDXGIFactory1_CreateSwapChain(factory, (IUnknown *)device, &desc, &swap);
    IDXGIOutput_Release(output);
    IDXGIAdapter1_Release(adapter);
    IDXGIFactory1_Release(factory);
#else
    hr = create(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0, 0, 0, D3D11_SDK_VERSION, &desc, &swap, &device, 0, &context);
#endif
    if (FAILED(hr)) {
        return 2;
    }
    DXGI_SWAP_CHAIN_DESC actual;
    if (FAILED(IDXGISwapChain_GetDesc(swap, &actual)) || actual.BufferCount != 2 || actual.Flags != desc.Flags ||
        actual.BufferDesc.Width != 320 || actual.BufferDesc.Height != 240) {
        return 20;
    }
    IDXGIOutput *containing = 0;
    if (FAILED(IDXGISwapChain_GetContainingOutput(swap, &containing)) || !containing) {
        return 21;
    }
#ifdef USE_DXGI_FACTORY
    if (containing != output) {
        return 21;
    }
#endif
    IDXGIOutput_Release(containing);
    ID3D11Texture2D *back = 0;
    if (SUCCEEDED(IDXGISwapChain_GetBuffer(swap, 1, relocation, (void **)&back)) || back) {
        return 22;
    }
    if (FAILED(IDXGISwapChain_GetBuffer(swap, 0, relocation, (void **)&back))) {
        return 3;
    }
    ID3D11ShaderResourceView *srv = 0;
    D3D11_SHADER_RESOURCE_VIEW_DESC srvdesc = { 0 };
    srvdesc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    srvdesc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
    srvdesc.Texture2D.MipLevels = 1;
    srvdesc.Texture2D.MostDetailedMip = 1;
    if (ID3D11Device_CreateShaderResourceView(device, (ID3D11Resource *)back, &srvdesc, &srv) != E_INVALIDARG || srv) {
        return 23;
    }
    if (ID3D11Device_CreateShaderResourceView(device, (ID3D11Resource *)back, 0, 0) != S_FALSE) {
        return 24;
    }
    if (FAILED(ID3D11Device_CreateShaderResourceView(device, (ID3D11Resource *)back, 0, &srv))) {
        return 25;
    }
    ID3D11ShaderResourceView_GetDesc(srv, &srvdesc);
    if (srvdesc.Format != desc.BufferDesc.Format || srvdesc.ViewDimension != D3D11_SRV_DIMENSION_TEXTURE2D ||
        srvdesc.Texture2D.MostDetailedMip || srvdesc.Texture2D.MipLevels != 1) {
        return 26;
    }
    ID3D11Resource *resource = 0;
    ID3D11ShaderResourceView_GetResource(srv, &resource);
    if (resource != (ID3D11Resource *)back) {
        return 27;
    }
    ID3D11Resource_Release(resource);
    ID3D11ShaderResourceView_Release(srv);
    ID3D11RenderTargetView *view = 0;
    if (FAILED(ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)back, 0, &view))) {
        return 4;
    }
    const float color[4] = { 0.25f, 0.5f, 0.75f, 1.0f };
    ID3D11DeviceContext_ClearRenderTargetView(context, view, color);
#ifndef NO_PRESENT
    if (FAILED(IDXGISwapChain_Present(swap, 0, 0))) {
        return 5;
    }
#endif
    ID3D11RenderTargetView_Release(view);
    ID3D11Texture2D_Release(back);
    IDXGISwapChain_Release(swap);
    ID3D11DeviceContext_Release(context);
    ID3D11Device_Release(device);
    return 0;
}
