#define COBJMACROS
#include <d3d11.h>
#include <windows.h>
static const unsigned vs[] = { 0x43425844, 0,          0,        0,  0,          1,        160,        2,          40,         108,
                               0x52444853, 60,         0x10040,  15, 0x0300005f, 0x1010f2, 0,          0x04000067, 0x1020f2,   0,
                               1,          0x05000036, 0x1020f2, 0,  0x101e46,   0,        0x0100003e, 0x4e475349, 44,         1,
                               8,          32,         0,        0,  3,          0,        0x0f0f,     0x49534f50, 0x4e4f4954, 0 };
static const unsigned ps[] = { 0x43425844, 0,      0,          0,  0,          1,          100,       1,          36,
                               0x52444853, 56,     0x40,       14, 0x03000065, 0x1020f2,   0,         0x08000036, 0x1020f2,
                               0,          0x4002, 0x3f800000, 0,  0,          0x3f800000, 0x0100003e };
/* D3D top half only: verifies viewport orientation and actual covered pixels. */
static float vertices[] = { -1, 1, 0, 1, -1, 0, 0, 1, 1, 1, 0, 1, 1, 0, 0, 1 };
static const unsigned sampled_ps[] = { 0x43425844, 0,      0,          0,        0,          1,          144,      1,          36,
                                       0x52444853, 100,    0x40,       25,       0x0300005a, 0x106000,   0,        0x04001858, 0x107000,
                                       0,          0x5555, 0x03000065, 0x1020f2, 0,          0x0c000045, 0x1020f2, 0,          0x4002,
                                       0,          0,      0,          0,        0x107e46,   0,          0x106000, 0,          0x0100003e };
static void *volatile anchor = vertices;
/* BC7 mode 6: equal endpoints, zero indices, 8-bit endpoints with p-bit zero. */
static void bc7_block(unsigned char *block, unsigned channel) {
    for (unsigned i = 0; i < 16; ++i) {
        block[i] = 0;
    }
    block[0] = 64;
    unsigned bit = 7;
    for (unsigned c = 0; c < 4; ++c) {
        for (unsigned e = 0; e < 2; ++e) {
            for (unsigned b = 0; b < 7; ++b, ++bit) {
                if (c == channel || c == 3) {
                    block[bit / 8] |= 1u << (bit % 8);
                }
            }
        }
    }
}
int entry(void) {
    PFN_D3D11_CREATE_DEVICE create = (void *)GetProcAddress(LoadLibraryA("d3d11.dll"), "D3D11CreateDevice");
    ID3D11Device *device = 0;
    ID3D11DeviceContext *context = 0;
    if (!create || FAILED(create(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0, 0, 0, 7, &device, 0, &context))) {
        return 1;
    }
    ID3D11VertexShader *vertex = 0;
    ID3D11PixelShader *pixel = 0;
    ID3D11InputLayout *layout = 0;
    if (FAILED(ID3D11Device_CreateVertexShader(device, vs, sizeof(vs), 0, &vertex)) ||
        FAILED(ID3D11Device_CreatePixelShader(device, ps, sizeof(ps), 0, &pixel))) {
        return 2;
    }
    D3D11_INPUT_ELEMENT_DESC element = { "POSITION", 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0 };
    if (FAILED(ID3D11Device_CreateInputLayout(device, &element, 1, vs, sizeof(vs), &layout))) {
        return 3;
    }
    D3D11_BUFFER_DESC bd = { sizeof(vertices), D3D11_USAGE_DEFAULT, D3D11_BIND_VERTEX_BUFFER, 0, 0, 0 };
    D3D11_SUBRESOURCE_DATA initial = { anchor, 0, 0 };
    ID3D11Buffer *buffer = 0;
    if (FAILED(ID3D11Device_CreateBuffer(device, &bd, &initial, &buffer))) {
        return 4;
    }
    D3D11_TEXTURE2D_DESC td = { 4, 4, 1, 1, DXGI_FORMAT_R8G8B8A8_UNORM, { 1, 0 }, D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET, 0, 0 };
    ID3D11Texture2D *target = 0, *staging = 0;
    ID3D11RenderTargetView *view = 0;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, 0, &target)) ||
        FAILED(ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)target, 0, &view))) {
        return 5;
    }
    float blue[] = { 0, 0, 1, 1 };
    ID3D11DeviceContext_ClearRenderTargetView(context, view, blue);
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, &view, 0);
    D3D11_VIEWPORT vp = { 0, 0, 4, 4, 0, 1 };
    ID3D11DeviceContext_RSSetViewports(context, 1, &vp);
    D3D11_RASTERIZER_DESC rd = { D3D11_FILL_SOLID, D3D11_CULL_NONE, FALSE, 0, 0, 0, TRUE, FALSE, TRUE, FALSE };
    ID3D11RasterizerState *raster = 0;
    if (FAILED(ID3D11Device_CreateRasterizerState(device, &rd, &raster))) {
        return 6;
    }
    ID3D11DeviceContext_RSSetState(context, raster);
    ID3D11DeviceContext_VSSetShader(context, vertex, 0, 0);
    ID3D11DeviceContext_PSSetShader(context, pixel, 0, 0);
    ID3D11DeviceContext_IASetInputLayout(context, layout);
    UINT stride = 16, offset = 0;
    ID3D11DeviceContext_IASetVertexBuffers(context, 0, 1, &buffer, &stride, &offset);
    ID3D11DeviceContext_IASetPrimitiveTopology(context, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    ID3D11DeviceContext_Draw(context, 4, 0);
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, 0, &staging))) {
        return 7;
    }
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)target);
    D3D11_MAPPED_SUBRESOURCE mapped;
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        return 8;
    }
    for (unsigned y = 0; y < 4; ++y) {
        for (unsigned x = 0; x < 4; ++x) {
            unsigned value = *(unsigned *)((char *)mapped.pData + y * mapped.RowPitch + x * 4);
            if (value != (y < 2 ? 0xff0000ffu : 0xffff0000u)) {
                return 9;
            }
        }
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    ID3D11PixelShader *sampled = 0;
    if (FAILED(ID3D11Device_CreatePixelShader(device, sampled_ps, sizeof(sampled_ps), 0, &sampled))) {
        return 10;
    }
    ID3D11DeviceContext_PSSetShader(context, sampled, 0, 0);
    unsigned green = 0xff00ff00;
    td.Width = td.Height = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    td.CPUAccessFlags = 0;
    ID3D11Texture2D *source = 0;
    ID3D11ShaderResourceView *srv = 0;
    PFN_D3D11_CREATE_DEVICE_AND_SWAP_CHAIN create_swap = (void *)GetProcAddress(LoadLibraryA("d3d11.dll"), "D3D11CreateDeviceAndSwapChain");
    DXGI_SWAP_CHAIN_DESC swapdesc = { 0 };
    swapdesc.BufferDesc.Width = swapdesc.BufferDesc.Height = 1;
    swapdesc.BufferDesc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    swapdesc.SampleDesc.Count = 1;
    swapdesc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT | DXGI_USAGE_SHADER_INPUT;
    swapdesc.BufferCount = 1;
    swapdesc.Windowed = TRUE;
    IDXGISwapChain *swap = 0;
    if (!create_swap || FAILED(create_swap(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0, 0, 0, 7, &swapdesc, &swap, 0, 0, 0))) {
        return 11;
    }
    static const GUID texture_iid = { 0x6f15aaf2, 0xd208, 0x4e89, { 0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c } };
    if (FAILED(IDXGISwapChain_GetBuffer(swap, 0, &texture_iid, (void **)&source))) {
        return 14;
    }
    ID3D11RenderTargetView *source_view = 0;
    if (FAILED(ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)source, 0, &source_view))) {
        return 15;
    }
    float green_color[] = { 0, 1, 0, 1 };
    ID3D11DeviceContext_ClearRenderTargetView(context, source_view, green_color);
    if (FAILED(ID3D11Device_CreateShaderResourceView(device, (ID3D11Resource *)source, 0, &srv))) {
        return 16;
    }
    for (unsigned pass = 0; pass < 2; ++pass) {
        ID3D11DeviceContext_PSSetShaderResources(context, 0, 1, pass ? &srv : 0);
        ID3D11DeviceContext_Draw(context, 4, 0);
        ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)target);
        if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
            return 12;
        }
        for (unsigned y = 0; y < 4; ++y) {
            for (unsigned x = 0; x < 4; ++x) {
                unsigned value = *(unsigned *)((char *)mapped.pData + y * mapped.RowPitch + x * 4);
                if (value != (y < 2 ? (pass ? green : 0) : 0xffff0000u)) {
                    return 13;
                }
            }
        }
        ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    }
    /* Present stored pixels, not only the clear-only swap-chain path. */
    swapdesc.BufferDesc.Width = swapdesc.BufferDesc.Height = 4;
    IDXGISwapChain *display = 0;
    ID3D11Texture2D *back = 0;
    if (FAILED(create_swap(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0, 0, 0, 7, &swapdesc, &display, 0, 0, 0))) {
        return 17;
    }
    if (FAILED(IDXGISwapChain_GetBuffer(display, 0, &texture_iid, (void **)&back))) {
        return 18;
    }
    ID3D11RenderTargetView *back_view = 0;
    if (FAILED(ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)back, 0, &back_view))) {
        return 20;
    }
    ID3D11DeviceContext_ClearRenderTargetView(context, back_view, blue);
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, &back_view, 0);
    /* This strip is front-facing in D3D; shader Y inversion reverses GL winding. */
    rd.CullMode = D3D11_CULL_BACK;
    rd.FrontCounterClockwise = TRUE;
    if (FAILED(ID3D11Device_CreateRasterizerState(device, &rd, &raster))) {
        return 28;
    }
    ID3D11DeviceContext_RSSetState(context, raster);
    ID3D11DeviceContext_Draw(context, 4, 0);
    if (FAILED(IDXGISwapChain_Present(display, 0, 0))) {
        return 19;
    }
    UINT support = 0;
    if (FAILED(ID3D11Device_CheckFormatSupport(device, DXGI_FORMAT_BC7_UNORM, &support)) ||
        !(support & D3D11_FORMAT_SUPPORT_SHADER_SAMPLE)) {
        return 21;
    }
    unsigned char block[16];
    bc7_block(block, 0);
    td.Width = td.Height = 4;
    td.Format = DXGI_FORMAT_BC7_UNORM;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    td.CPUAccessFlags = 0;
    initial.pSysMem = block;
    initial.SysMemPitch = 16;
    ID3D11Texture2D *compressed = 0, *compressed_readback = 0;
    ID3D11ShaderResourceView *compressed_view = 0;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, &initial, &compressed)) ||
        FAILED(ID3D11Device_CreateShaderResourceView(device, (ID3D11Resource *)compressed, 0, &compressed_view))) {
        return 22;
    }
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, 0, &compressed_readback))) {
        return 23;
    }
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, &view, 0);
    ID3D11DeviceContext_PSSetShaderResources(context, 0, 1, &compressed_view);
    for (unsigned pass = 0; pass < 2; ++pass) {
        if (pass) {
            bc7_block(block, 2);
            D3D11_BOX box = { 0, 0, 0, 4, 4, 1 };
            ID3D11DeviceContext_UpdateSubresource(context, (ID3D11Resource *)compressed, 0, &box, block, 16, 0);
        }
        ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)compressed_readback, (ID3D11Resource *)compressed);
        if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)compressed_readback, 0, D3D11_MAP_READ, 0, &mapped)) ||
            mapped.RowPitch != 16 || mapped.DepthPitch != 16) {
            return 24;
        }
        for (unsigned i = 0; i < 16; ++i) {
            if (((unsigned char *)mapped.pData)[i] != block[i]) {
                return 25;
            }
        }
        ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)compressed_readback, 0);
        ID3D11DeviceContext_Draw(context, 4, 0);
        ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)target);
        if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
            return 26;
        }
        for (unsigned y = 0; y < 4; ++y) {
            for (unsigned x = 0; x < 4; ++x) {
                unsigned value = *(unsigned *)((char *)mapped.pData + y * mapped.RowPitch + x * 4);
                if (value != (y < 2 ? (pass ? 0xfefe0000u : 0xfe0000feu) : 0xffff0000u)) {
                    return 27;
                }
            }
        }
        ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    }
    /* Rounded block rows, source pitch padding, and a partial edge block. */
    unsigned char padded[80];
    for (unsigned i = 0; i < 80; ++i) {
        padded[i] = 0xcc;
    }
    for (unsigned y = 0; y < 2; ++y) {
        for (unsigned x = 0; x < 2; ++x) {
            bc7_block(padded + y * 48 + x * 16, 0);
        }
    }
    td.Width = td.Height = 5;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    td.CPUAccessFlags = 0;
    initial.pSysMem = padded;
    initial.SysMemPitch = 48;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, &initial, &compressed))) {
        return 29;
    }
    D3D11_BOX edge = { 4, 4, 0, 5, 5, 1 };
    ID3D11DeviceContext_UpdateSubresource(context, (ID3D11Resource *)compressed, 0, &edge, block, 16, 0);
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, 0, &compressed_readback))) {
        return 30;
    }
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)compressed_readback, (ID3D11Resource *)compressed);
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)compressed_readback, 0, D3D11_MAP_READ, 0, &mapped)) ||
        mapped.RowPitch != 32 || mapped.DepthPitch != 64) {
        return 31;
    }
    for (unsigned y = 0; y < 2; ++y) {
        for (unsigned x = 0; x < 2; ++x) {
            for (unsigned i = 0; i < 16; ++i) {
                if (((unsigned char *)mapped.pData)[y * 32 + x * 16 + i] != (y && x ? block[i] : padded[y * 48 + x * 16 + i])) {
                    return 32;
                }
            }
        }
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)compressed_readback, 0);
    /* Odd-width D16 rows, cross-draw occlusion, clears, and Copy/Map coherence. */
    td.Width = 3;
    td.Height = 4;
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET;
    td.CPUAccessFlags = 0;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, 0, &target)) ||
        FAILED(ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)target, 0, &view))) {
        return 33;
    }
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, 0, &staging))) {
        return 34;
    }
    td.Format = DXGI_FORMAT_D16_UNORM;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_DEPTH_STENCIL;
    td.CPUAccessFlags = 0;
    ID3D11Texture2D *depth = 0, *depth_readback = 0;
    ID3D11DepthStencilView *dsv = 0, *readonly = 0;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, 0, &depth)) ||
        FAILED(ID3D11Device_CreateDepthStencilView(device, (ID3D11Resource *)depth, 0, &dsv))) {
        return 35;
    }
    D3D11_DEPTH_STENCIL_VIEW_DESC dd = { DXGI_FORMAT_D16_UNORM, D3D11_DSV_DIMENSION_TEXTURE2D, D3D11_DSV_READ_ONLY_DEPTH, { 0 } };
    if (FAILED(ID3D11Device_CreateDepthStencilView(device, (ID3D11Resource *)depth, &dd, &readonly))) {
        return 36;
    }
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, 0, &depth_readback))) {
        return 37;
    }
    vp.Width = 3;
    ID3D11DeviceContext_RSSetViewports(context, 1, &vp);
    /* clear<0 preserves prior depth. Color is cleared blue before each draw. */
    static const struct {
        float clear, z;
        unsigned func, write, ro, enabled, pass, stored;
    } cases[] = { { 1, .25f, 2, 1, 0, 1, 1, 16384 },   { -1, .75f, 2, 1, 0, 1, 0, 16384 }, { -1, .125f, 2, 0, 0, 1, 1, 16384 },
                  { -1, .125f, 2, 1, 1, 1, 1, 16384 }, { -1, .75f, 5, 1, 0, 1, 1, 49151 }, { 0, .25f, 2, 1, 0, 1, 0, 0 },
                  { 0, 1, 1, 1, 0, 1, 0, 0 },          { 0, 0, 3, 1, 0, 1, 1, 0 },         { 0, 0, 4, 1, 0, 1, 1, 0 },
                  { 0, 1, 6, 1, 0, 1, 1, 65535 },      { 0, 0, 7, 1, 0, 1, 1, 0 },         { 0, 1, 8, 1, 0, 1, 1, 65535 },
                  { 0, 1, 2, 1, 0, 0, 1, 0 },          { 1, 0, 0, 1, 0, 1, 1, 0 } };
    unsigned untouched = 0;
    for (unsigned i = 0; i < sizeof(cases) / sizeof(cases[0]); ++i) {
        if (i == 7) { /* Final release invalidates the cached shader pair. */
            ID3D11PixelShader_Release(pixel);
            ID3D11DeviceContext_PSSetShader(context, 0, 0, 0);
            if (FAILED(ID3D11Device_CreatePixelShader(device, ps, sizeof(ps), 0, &pixel))) {
                return 75;
            }
        }
        if (cases[i].clear >= 0) {
            ID3D11DeviceContext_ClearDepthStencilView(context, dsv, D3D11_CLEAR_DEPTH, cases[i].clear, 0);
            untouched = cases[i].clear ? 65535 : 0;
        }
        ID3D11DeviceContext_ClearRenderTargetView(context, view, blue);
        ID3D11DeviceContext_OMSetRenderTargets(context, 1, &view, cases[i].ro ? readonly : dsv);
        D3D11_DEPTH_STENCIL_DESC ds = { 0 };
        ds.DepthEnable = cases[i].enabled;
        ds.DepthWriteMask = cases[i].write;
        ds.DepthFunc = cases[i].func;
        ID3D11DepthStencilState *state = 0;
        if (cases[i].func && FAILED(ID3D11Device_CreateDepthStencilState(device, &ds, &state))) {
            return 38;
        }
        ID3D11DeviceContext_OMSetDepthStencilState(context, state, 0);
        if (state) {
            ID3D11DepthStencilState_Release(state);
        }
        for (unsigned j = 0; j < 4; ++j) {
            vertices[j * 4 + 2] = cases[i].z;
        }
        ID3D11DeviceContext_UpdateSubresource(context, (ID3D11Resource *)buffer, 0, 0, vertices, 0, 0);
        ID3D11DeviceContext_PSSetShader(context, pixel, 0, 0);
        ID3D11DeviceContext_Draw(context, 4, 0);
        if (i == 13) {
            for (unsigned repeat = 0; repeat < 32; ++repeat) {
                /* Repeated clears must retain GPU storage and cached attachments. */
                ID3D11DeviceContext_ClearRenderTargetView(context, view, blue);
                ID3D11DeviceContext_ClearDepthStencilView(context, dsv, D3D11_CLEAR_DEPTH, 1, 0);
                ID3D11DeviceContext_Draw(context, 4, 0);
            }
        }
        ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)target);
        if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
            return 39;
        }
        for (unsigned y = 0; y < 4; ++y) {
            for (unsigned x = 0; x < 3; ++x) {
                if (*(unsigned *)((char *)mapped.pData + y * mapped.RowPitch + x * 4) !=
                    (y < 2 && cases[i].pass ? 0xff0000ffu : 0xffff0000u)) {
                    return 40 + i;
                }
            }
        }
        ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
        ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)depth_readback, (ID3D11Resource *)depth);
        if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)depth_readback, 0, D3D11_MAP_READ, 0, &mapped)) ||
            mapped.RowPitch != 6) {
            return 60;
        }
        for (unsigned y = 0; y < 4; ++y) {
            for (unsigned x = 0; x < 3; ++x) {
                int value = *(unsigned short *)((char *)mapped.pData + y * mapped.RowPitch + x * 2),
                    want = y < 2 ? cases[i].stored : untouched;
                if (value - want > 1 || want - value > 1) {
                    return 61 + i;
                }
            }
        }
        ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)depth_readback, 0);
    }
    /* A boxed CPU update must preserve untouched pixels from the GPU draw. */
    D3D11_BOX patch = { 1, 3, 0, 2, 4, 1 };
    ID3D11DeviceContext_UpdateSubresource(context, (ID3D11Resource *)target, 0, &patch, &green, 4, 0);
    /* Recreated GPU storage must not reuse an FBO holding its deleted texture. */
    ID3D11DeviceContext_Draw(context, 4, 0);
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)target);
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        return 76;
    }
    for (unsigned y = 0; y < 4; ++y) {
        for (unsigned x = 0; x < 3; ++x) {
            if (*(unsigned *)((char *)mapped.pData + y * mapped.RowPitch + x * 4) != (y == 3 && x == 1 ? green
                                                                                      : y < 2          ? 0xff0000ffu
                                                                                                       : 0xffff0000u)) {
                return 77;
            }
        }
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    /* Final release and object-slot reuse must invalidate cached attachments. */
    ID3D11DeviceContext_OMSetRenderTargets(context, 0, 0, 0);
    ID3D11RenderTargetView_Release(view);
    ID3D11Texture2D_Release(target);
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET;
    td.CPUAccessFlags = 0;
    if (FAILED(ID3D11Device_CreateTexture2D(device, &td, 0, &target)) ||
        FAILED(ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)target, 0, &view))) {
        return 78;
    }
    ID3D11DeviceContext_ClearRenderTargetView(context, view, blue);
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, &view, 0);
    ID3D11DeviceContext_Draw(context, 4, 0);
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)target);
    if (FAILED(ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        return 79;
    }
    for (unsigned y = 0; y < 4; ++y) {
        for (unsigned x = 0; x < 3; ++x) {
            if (*(unsigned *)((char *)mapped.pData + y * mapped.RowPitch + x * 4) != (y < 2 ? 0xff0000ffu : 0xffff0000u)) {
                return 80;
            }
        }
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    return 0;
}
