#define COBJMACROS
#include <d3d11.h>
#include <windows.h>
static unsigned anchor;
static unsigned *volatile relocated = &anchor;
int entry(void) {
    *relocated = 1;
    PFN_D3D11_CREATE_DEVICE create = (void *)GetProcAddress(LoadLibraryA("d3d11.dll"), "D3D11CreateDevice");
    ID3D11Device *device = 0;
    ID3D11DeviceContext *context = 0;
    ID3D11BlendState *state = 0, *copy = 0, *got = 0;
    if (!create || FAILED(create(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0, 0, 0, 7, &device, 0, &context))) {
        return 1;
    }
    D3D11_BLEND_DESC desc = { 0 };
    desc.RenderTarget[0] = (D3D11_RENDER_TARGET_BLEND_DESC){
        TRUE, D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_OP_ADD, D3D11_BLEND_ONE, D3D11_BLEND_ZERO, D3D11_BLEND_OP_MAX,
        15
    };
    if (ID3D11Device_CreateBlendState(device, &desc, 0) != S_FALSE) {
        return 2;
    }
    if (FAILED(ID3D11Device_CreateBlendState(device, &desc, &state))) {
        return 3;
    }
    if (FAILED(ID3D11Device_CreateBlendState(device, &desc, &copy)) || copy != state) {
        return 4;
    }
    ID3D11BlendState_Release(copy);
    D3D11_BLEND_DESC roundtrip;
    ID3D11BlendState_GetDesc(state, &roundtrip);
    if (roundtrip.RenderTarget[0].SrcBlend != desc.RenderTarget[0].SrcBlend ||
        roundtrip.RenderTarget[0].BlendOpAlpha != D3D11_BLEND_OP_MAX) {
        return 5;
    }
    float factors[4] = { 0.25f, 0.5f, 0.75f, 1 }, returned[4];
    UINT mask = 0;
    ID3D11DeviceContext_OMSetBlendState(context, state, factors, 0x55);
    ID3D11BlendState_Release(state);
    ID3D11DeviceContext_OMGetBlendState(context, &got, returned, &mask);
    if (got != state || mask != 0x55) {
        return 6;
    }
    for (int i = 0; i < 4; ++i) {
        if (returned[i] != factors[i]) {
            return 7;
        }
    }
    ID3D11DeviceContext_OMSetBlendState(context, got, 0, ~0u);
    ID3D11BlendState_Release(got);
    ID3D11DeviceContext_OMGetBlendState(context, 0, returned, &mask);
    if (mask != ~0u || returned[0] != 1 || returned[3] != 1) {
        return 8;
    }
    ID3D11DeviceContext_OMSetBlendState(context, 0, 0, ~0u);
    ID3D11DeviceContext_OMGetBlendState(context, &got, 0, 0);
    if (got) {
        return 9;
    }
    desc.RenderTarget[0].BlendOp = 0;
    if (ID3D11Device_CreateBlendState(device, &desc, &got) != E_INVALIDARG || got) {
        return 10;
    }
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = 12;
    if (ID3D11Device_CreateBlendState(device, &desc, &got) != E_INVALIDARG || got) {
        return 11;
    }
    D3D11_SAMPLER_DESC sampler = { D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                                   D3D11_TEXTURE_ADDRESS_WRAP,
                                   D3D11_TEXTURE_ADDRESS_CLAMP,
                                   D3D11_TEXTURE_ADDRESS_MIRROR,
                                   0,
                                   1,
                                   D3D11_COMPARISON_NEVER,
                                   { 0, 0, 0, 0 },
                                   0,
                                   16 };
    ID3D11SamplerState *sample = 0, *duplicate = 0;
    D3D11_SAMPLER_DESC returned_sampler;
    if (ID3D11Device_CreateSamplerState(device, &sampler, 0) != S_FALSE) {
        return 12;
    }
    if (FAILED(ID3D11Device_CreateSamplerState(device, &sampler, &sample))) {
        return 13;
    }
    if (FAILED(ID3D11Device_CreateSamplerState(device, &sampler, &duplicate)) || sample != duplicate) {
        return 14;
    }
    ID3D11SamplerState_GetDesc(sample, &returned_sampler);
    if (returned_sampler.Filter != sampler.Filter || returned_sampler.AddressW != sampler.AddressW || returned_sampler.MaxLOD != 16) {
        return 15;
    }
    ID3D11SamplerState_Release(duplicate);
    ID3D11DeviceContext_PSSetSamplers(context, 2, 1, &sample);
    ID3D11DeviceContext_VSSetSamplers(context, 3, 1, &sample);
    ID3D11SamplerState_Release(sample);
    ID3D11DeviceContext_PSGetSamplers(context, 2, 1, &duplicate);
    if (duplicate != sample) {
        return 39;
    }
    ID3D11DeviceContext_PSSetSamplers(context, 2, 1, &duplicate);
    ID3D11SamplerState_Release(duplicate);
    ID3D11DeviceContext_PSSetSamplers(context, 2, 1, 0);
    ID3D11DeviceContext_VSGetSamplers(context, 3, 1, &duplicate);
    if (duplicate != sample) {
        return 40;
    }
    ID3D11SamplerState_Release(duplicate);
    ID3D11DeviceContext_VSSetSamplers(context, 3, 1, 0);
    sample = 0;
    sampler.Filter = 2;
    if (ID3D11Device_CreateSamplerState(device, &sampler, &sample) != E_INVALIDARG || sample) {
        return 16;
    }
    sampler.Filter = D3D11_FILTER_ANISOTROPIC;
    sampler.MaxAnisotropy = 17;
    if (ID3D11Device_CreateSamplerState(device, &sampler, &sample) != E_INVALIDARG || sample) {
        return 17;
    }
    sampler.MaxAnisotropy = 1;
    sampler.MinLOD = 32;
    if (ID3D11Device_CreateSamplerState(device, &sampler, &sample) != E_INVALIDARG || sample) {
        return 18;
    }
    D3D11_RASTERIZER_DESC raster = { D3D11_FILL_SOLID, D3D11_CULL_BACK, TRUE, 2, 0, 1, TRUE, TRUE, FALSE, FALSE }, raster_got;
    ID3D11RasterizerState *rs = 0, *rs_copy = 0;
    if (ID3D11Device_CreateRasterizerState(device, &raster, 0) != S_FALSE) {
        return 19;
    }
    if (FAILED(ID3D11Device_CreateRasterizerState(device, &raster, &rs))) {
        return 20;
    }
    if (FAILED(ID3D11Device_CreateRasterizerState(device, &raster, &rs_copy)) || rs_copy != rs) {
        return 21;
    }
    ID3D11RasterizerState_Release(rs_copy);
    ID3D11RasterizerState_GetDesc(rs, &raster_got);
    if (raster_got.DepthBias != 2 || !raster_got.FrontCounterClockwise || !raster_got.ScissorEnable) {
        return 22;
    }
    ID3D11DeviceContext_RSSetState(context, rs);
    ID3D11RasterizerState_Release(rs);
    ID3D11DeviceContext_RSGetState(context, &rs_copy);
    if (rs_copy != rs) {
        return 23;
    }
    ID3D11DeviceContext_RSSetState(context, rs_copy);
    ID3D11RasterizerState_Release(rs_copy);
    ID3D11DeviceContext_RSSetState(context, 0);
    ID3D11DeviceContext_RSGetState(context, &rs_copy);
    if (rs_copy) {
        return 24;
    }
    raster.FillMode = 0;
    if (ID3D11Device_CreateRasterizerState(device, &raster, &rs_copy) != E_INVALIDARG || rs_copy) {
        return 25;
    }
    D3D11_DEPTH_STENCIL_DESC depth = { TRUE,
                                       D3D11_DEPTH_WRITE_MASK_ALL,
                                       D3D11_COMPARISON_LESS,
                                       TRUE,
                                       0x7f,
                                       0x3f,
                                       { D3D11_STENCIL_OP_KEEP, D3D11_STENCIL_OP_INCR_SAT, D3D11_STENCIL_OP_REPLACE,
                                         D3D11_COMPARISON_ALWAYS },
                                       { D3D11_STENCIL_OP_ZERO, D3D11_STENCIL_OP_DECR, D3D11_STENCIL_OP_INVERT, D3D11_COMPARISON_EQUAL } },
                             depth_got;
    ID3D11DepthStencilState *ds = 0, *ds_copy = 0;
    if (ID3D11Device_CreateDepthStencilState(device, &depth, 0) != S_FALSE) {
        return 26;
    }
    if (FAILED(ID3D11Device_CreateDepthStencilState(device, &depth, &ds))) {
        return 27;
    }
    if (FAILED(ID3D11Device_CreateDepthStencilState(device, &depth, &ds_copy)) || ds_copy != ds) {
        return 28;
    }
    ID3D11DepthStencilState_Release(ds_copy);
    ID3D11DepthStencilState_GetDesc(ds, &depth_got);
    if (depth_got.StencilReadMask != 0x7f || depth_got.BackFace.StencilDepthFailOp != D3D11_STENCIL_OP_DECR) {
        return 29;
    }
    ID3D11DeviceContext_OMSetDepthStencilState(context, ds, 0x123);
    ID3D11DepthStencilState_Release(ds);
    ID3D11DeviceContext_OMGetDepthStencilState(context, &ds_copy, &mask);
    if (ds_copy != ds || mask != 0x123) {
        return 30;
    }
    ID3D11DeviceContext_OMSetDepthStencilState(context, ds_copy, 7);
    ID3D11DepthStencilState_Release(ds_copy);
    ID3D11DeviceContext_OMSetDepthStencilState(context, 0, 0);
    ID3D11DeviceContext_OMGetDepthStencilState(context, &ds_copy, &mask);
    if (ds_copy || mask) {
        return 31;
    }
    depth.FrontFace.StencilFunc = 0;
    if (ID3D11Device_CreateDepthStencilState(device, &depth, &ds_copy) != E_INVALIDARG || ds_copy) {
        return 32;
    }
    D3D11_VIEWPORT ports[2] = { { 0.5f, 1.5f, 320, 240, 0.25f, 0.75f }, { 5, 6, 100, 80, 0, 1 } }, port_got[3];
    UINT count = 0;
    ID3D11DeviceContext_RSSetViewports(context, 2, ports);
    ID3D11DeviceContext_RSGetViewports(context, &count, 0);
    if (count != 2) {
        return 33;
    }
    count = 3;
    ID3D11DeviceContext_RSGetViewports(context, &count, port_got);
    if (count != 2 || port_got[0].TopLeftX != 0.5f || port_got[0].MinDepth != 0.25f || port_got[1].Width != 100 || port_got[2].Width) {
        return 34;
    }
    count = 1;
    ID3D11DeviceContext_RSGetViewports(context, &count, port_got);
    if (count != 2 || port_got[0].Height != 240) {
        return 35;
    }
    D3D11_RECT rects[2] = { { 1, 2, 30, 40 }, { -5, -6, 10, 20 } }, rect_got[3];
    ID3D11DeviceContext_RSSetScissorRects(context, 2, rects);
    count = 3;
    ID3D11DeviceContext_RSGetScissorRects(context, &count, rect_got);
    if (count != 2 || rect_got[1].left != -5 || rect_got[0].bottom != 40 || rect_got[2].right) {
        return 36;
    }
    ID3D11DeviceContext_RSSetViewports(context, 0, 0);
    ID3D11DeviceContext_RSSetScissorRects(context, 0, 0);
    ID3D11DeviceContext_RSGetViewports(context, &count, 0);
    if (count) {
        return 37;
    }
    ID3D11DeviceContext_RSGetScissorRects(context, &count, 0);
    if (count) {
        return 38;
    }
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
    if (FAILED(ID3D11Device_CreateBlendState(device, &desc, &state))) {
        return 41;
    }
    ID3D11DeviceContext_OMSetBlendState(context, state, factors, 0x55);
    ID3D11BlendState_Release(state);
    ID3D11DeviceContext_RSSetViewports(context, 2, ports);
    ID3D11DeviceContext_IASetPrimitiveTopology(context, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ID3D11DeviceContext_ClearState(context);
    ID3D11DeviceContext_Flush(context);
    ID3D11DeviceContext_OMGetBlendState(context, &got, returned, &mask);
    if (got || mask != ~0u || returned[0] != 1) {
        return 42;
    }
    count = 0;
    ID3D11DeviceContext_RSGetViewports(context, &count, 0);
    if (count) {
        return 43;
    }
    D3D11_PRIMITIVE_TOPOLOGY primitive;
    ID3D11DeviceContext_IAGetPrimitiveTopology(context, &primitive);
    if (primitive != D3D11_PRIMITIVE_TOPOLOGY_UNDEFINED) {
        return 44;
    }
    /* Repeated state creation must reclaim released objects and method entries. */
    for (unsigned i = 0; i < 3000; ++i) {
        desc.RenderTarget[0].RenderTargetWriteMask = i % 16;
        if (FAILED(ID3D11Device_CreateBlendState(device, &desc, &state))) {
            return 45;
        }
        ID3D11DeviceContext_OMSetBlendState(context, state, 0, ~0u);
        ID3D11BlendState_Release(state);
        ID3D11DeviceContext_OMGetBlendState(context, &got, 0, 0);
        ID3D11BlendState_GetDesc(got, &roundtrip);
        if (roundtrip.RenderTarget[0].RenderTargetWriteMask != i % 16) {
            return 46;
        }
        ID3D11BlendState_Release(got);
        ID3D11DeviceContext_OMSetBlendState(context, 0, 0, ~0u);
    }
    ID3D11DeviceContext_Release(context);
    ID3D11Device_Release(device);
    return 0;
}
