#define COBJMACROS
#include <d3d11.h>
#include <windows.h>
/* Synthetic vertex bytecode plus one POSITION float4 input signature. */
static const unsigned shader[] = { 0x43425844, 0,          0,        0,  0,          1,        160,        2,          40,         108,
                                   0x52444853, 60,         0x10040,  15, 0x0300005f, 0x1010f2, 0,          0x04000067, 0x1020f2,   0,
                                   1,          0x05000036, 0x1020f2, 0,  0x101e46,   0,        0x0100003e, 0x4e475349, 44,         1,
                                   8,          32,         0,        0,  3,          0,        0x0f0f,     0x49534f50, 0x4e4f4954, 0 };
static const void *volatile bytecode = shader;
int entry(void) {
    PFN_D3D11_CREATE_DEVICE create = (void *)GetProcAddress(LoadLibraryA("d3d11.dll"), "D3D11CreateDevice");
    ID3D11Device *device = 0, *got = 0;
    ID3D11InputLayout *layout = 0;
    if (!create || FAILED(create(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0, 0, 0, 7, &device, 0, 0))) {
        return 1;
    }
    D3D11_INPUT_ELEMENT_DESC element = {
        "position", 0, DXGI_FORMAT_R32G32B32_FLOAT, 0, D3D11_APPEND_ALIGNED_ELEMENT, D3D11_INPUT_PER_VERTEX_DATA, 0
    };
    if (ID3D11Device_CreateInputLayout(device, &element, 1, bytecode, sizeof(shader), 0) != S_FALSE) {
        return 2;
    }
    if (FAILED(ID3D11Device_CreateInputLayout(device, &element, 1, bytecode, sizeof(shader), &layout)) || !layout) {
        return 3;
    }
    ID3D11InputLayout_GetDevice(layout, &got);
    if (got != device) {
        return 4;
    }
    ID3D11DeviceContext *context = 0;
    ID3D11Device_GetImmediateContext(device, &context);
    ID3D11DeviceContext_IASetInputLayout(context, layout);
    ID3D11InputLayout_Release(layout);
    ID3D11InputLayout *bound = 0;
    ID3D11DeviceContext_IAGetInputLayout(context, &bound);
    if (bound != layout) {
        return 12;
    }
    ID3D11DeviceContext_IASetInputLayout(context, 0);
    ID3D11InputLayout_Release(bound);
    ID3D11DeviceContext_Release(context);
    ID3D11Device_Release(got);
    layout = 0;
    element.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    if (FAILED(ID3D11Device_CreateInputLayout(device, &element, 1, bytecode, sizeof(shader), &layout))) {
        return 9;
    }
    ID3D11InputLayout_Release(layout);
    layout = 0;
    element.Format = DXGI_FORMAT_R32G32B32_FLOAT;
    element.SemanticName = "MISSING";
    if (ID3D11Device_CreateInputLayout(device, &element, 1, bytecode, sizeof(shader), &layout) != E_INVALIDARG || layout) {
        return 5;
    }
    element.SemanticName = "POSITION";
    element.AlignedByteOffset = 2044;
    if (ID3D11Device_CreateInputLayout(device, &element, 1, bytecode, sizeof(shader), &layout) != E_INVALIDARG || layout) {
        return 6;
    }
    element.AlignedByteOffset = 0;
    element.InputSlot = 32;
    if (ID3D11Device_CreateInputLayout(device, &element, 1, bytecode, sizeof(shader), &layout) != E_INVALIDARG || layout) {
        return 7;
    }
    element.InputSlot = 0;
    element.Format = DXGI_FORMAT_D24_UNORM_S8_UINT;
    if (ID3D11Device_CreateInputLayout(device, &element, 1, bytecode, sizeof(shader), &layout) != E_INVALIDARG || layout) {
        return 8;
    }
    element.Format = DXGI_FORMAT_R32G32B32_FLOAT;
    unsigned damaged[sizeof(shader) / 4];
    for (unsigned i = 0; i < sizeof(shader) / 4; ++i) {
        damaged[i] = shader[i];
    }
    damaged[31] = 0xffffffffu; /* ISGN semantic string outside its chunk. */
    if (ID3D11Device_CreateInputLayout(device, &element, 1, damaged, sizeof(damaged), &layout) != E_INVALIDARG || layout) {
        return 10;
    }
    D3D11_INPUT_ELEMENT_DESC duplicates[2] = { element, element };
    if (ID3D11Device_CreateInputLayout(device, duplicates, 2, bytecode, sizeof(shader), &layout) != E_INVALIDARG || layout) {
        return 11;
    }
    ID3D11Device_Release(device);
    return 0;
}
