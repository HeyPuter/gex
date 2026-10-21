#define COBJMACROS
#include <d3d11.h>
#include <windows.h>
/* Synthetic vs_4_0: POSITION -> SV_POSITION, translated rather than substituted. */
static const unsigned code[] = { 0x43425844, 0,  0,          0,          0,          1,          104, 1,          36,
                                 0x52444853, 60, 0x10040,    15,         0x0300005f, 0x001010f2, 0,   0x04000067, 0x001020f2,
                                 0,          1,  0x05000036, 0x001020f2, 0,          0x00101e46, 0,   0x0100003e };
static const void *volatile bytecode = code;
static const unsigned pixel[] = { 0x43425844, 0,          0,          0,          0,          1,          148,        1,
                                  36,         0x52444853, 104,        0x40,       26,         0x03001062, 0x001010f2, 0,
                                  0x03000065, 0x001020f2, 0,          0x02000068, 1,          0x0700001d, 0x00100012, 0,
                                  0x0010100a, 0,          0x4001,     0,          0x0304000d, 0x0010000a, 0,          0x05000036,
                                  0x001020f2, 0,          0x00101e46, 0,          0x0100003e };
int entry(void) {
    PFN_D3D11_CREATE_DEVICE create = (void *)GetProcAddress(LoadLibraryA("d3d11.dll"), "D3D11CreateDevice");
    ID3D11Device *device = 0, *queried = 0;
    ID3D11VertexShader *shader = 0;
    if (!create || FAILED(create(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0, 0, 0, 7, &device, 0, 0))) {
        return 1;
    }
    if (ID3D11Device_CreateVertexShader(device, bytecode, sizeof(code), 0, 0) != S_FALSE) {
        return 2;
    }
    if (FAILED(ID3D11Device_CreateVertexShader(device, bytecode, sizeof(code), 0, &shader)) || !shader) {
        return 3;
    }
    ID3D11VertexShader_GetDevice(shader, &queried);
    if (queried != device) {
        return 4;
    }
    ID3D11PixelShader *fragment = 0;
    if (FAILED(ID3D11Device_CreatePixelShader(device, pixel, sizeof(pixel), 0, &fragment)) || !fragment) {
        return 5;
    }
    ID3D11DeviceContext *context = 0;
    ID3D11Device_GetImmediateContext(device, &context);
    ID3D11DeviceContext_VSSetShader(context, shader, 0, 0);
    ID3D11DeviceContext_PSSetShader(context, fragment, 0, 0);
    ID3D11PixelShader_Release(fragment);
    ID3D11VertexShader *bound = 0;
    UINT classes = 9;
    ID3D11DeviceContext_VSGetShader(context, &bound, 0, &classes);
    if (bound != shader || classes) {
        return 6;
    }
    ID3D11DeviceContext_VSSetShader(context, bound, 0, 0);
    ID3D11VertexShader_Release(bound);
    ID3D11PixelShader *bound_pixel = 0;
    ID3D11DeviceContext_PSGetShader(context, &bound_pixel, 0, 0);
    if (bound_pixel != fragment) {
        return 7;
    }
    ID3D11PixelShader_Release(bound_pixel);
    ID3D11DeviceContext_PSSetShader(context, 0, 0, 0);
    ID3D11DeviceContext_VSSetShader(context, 0, 0, 0);
    ID3D11DeviceContext_GSSetShader(context, 0, 0, 0);
    ID3D11DeviceContext_HSSetShader(context, 0, 0, 0);
    ID3D11DeviceContext_DSSetShader(context, 0, 0, 0);
    ID3D11DeviceContext_CSSetShader(context, 0, 0, 0);
    ID3D11HullShader *hull = 0;
    classes = 4;
    ID3D11DeviceContext_HSGetShader(context, &hull, 0, &classes);
    if (hull || classes) {
        return 8;
    }
    ID3D11DeviceContext_Release(context);
    ID3D11Device_Release(queried);
    ID3D11VertexShader_Release(shader);
    ID3D11Device_Release(device);
    return 0;
}
