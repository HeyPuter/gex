// COM methods that return a struct, called from C++: a WIC bitmap render
// target's GetSize, GetPixelSize and GetPixelFormat, a solid brush's
// GetColor, and the bitmap's own GetSize and GetPixelSize. Windows (and
// Wine's C implementations) return the struct through a pointer the method
// also returns; C++ calls them by value through the headers' inline
// wrappers. Prints "ok ..." lines and "0 failures".
#include <windows.h>
#include <d2d1.h>
#include <wincodec.h>
#include <stdio.h>

static int failures;
#define CHECK(c, ...) do { if (c) printf("ok "); else { printf("FAIL "); failures++; } printf(__VA_ARGS__); printf("\n"); } while (0)

int main()
{
    IWICImagingFactory *wic = nullptr;
    IWICBitmap *bmp = nullptr;
    ID2D1Factory *d2d = nullptr;
    ID2D1RenderTarget *rt = nullptr;
    HRESULT hr;

    setvbuf(stdout, nullptr, _IONBF, 0);
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    hr = CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER, IID_IWICImagingFactory, (void **)&wic);
    CHECK(SUCCEEDED(hr), "a WIC factory (%#lx)", (unsigned long)hr);
    if (FAILED(hr)) return 1;
    hr = wic->CreateBitmap(160, 96, GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad, &bmp);
    CHECK(SUCCEEDED(hr), "a 160x96 WIC bitmap (%#lx)", (unsigned long)hr);
    hr = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory), nullptr, (void **)&d2d);
    CHECK(SUCCEEDED(hr), "a Direct2D factory (%#lx)", (unsigned long)hr);
    if (!bmp || !d2d) return 1;

    D2D1_RENDER_TARGET_PROPERTIES props = {};
    props.pixelFormat.format = DXGI_FORMAT_B8G8R8A8_UNORM;
    props.pixelFormat.alphaMode = D2D1_ALPHA_MODE_PREMULTIPLIED;
    props.dpiX = props.dpiY = 192.0f;
    hr = d2d->CreateWicBitmapRenderTarget(bmp, &props, &rt);
    CHECK(SUCCEEDED(hr), "a WIC bitmap render target (%#lx)", (unsigned long)hr);
    if (!rt) return 1;

    D2D1_SIZE_F size = rt->GetSize();
    CHECK(size.width == 80.0f && size.height == 48.0f, "GetSize %.1fx%.1f (160x96 pixels at 192 dpi)", size.width, size.height);
    D2D1_SIZE_U px = rt->GetPixelSize();
    CHECK(px.width == 160 && px.height == 96, "GetPixelSize %ux%u", px.width, px.height);
    D2D1_PIXEL_FORMAT fmt = rt->GetPixelFormat();
    CHECK(fmt.format == DXGI_FORMAT_B8G8R8A8_UNORM && fmt.alphaMode == D2D1_ALPHA_MODE_PREMULTIPLIED,
          "GetPixelFormat format %d alpha mode %d", (int)fmt.format, (int)fmt.alphaMode);

    ID2D1SolidColorBrush *brush = nullptr;
    D2D1_COLOR_F red = { 1.0f, 0.25f, 0.5f, 0.75f };
    hr = rt->CreateSolidColorBrush(&red, nullptr, &brush);
    CHECK(SUCCEEDED(hr), "a solid brush (%#lx)", (unsigned long)hr);
    if (brush)
    {
        D2D1_COLOR_F c = brush->GetColor();
        CHECK(c.r == 1.0f && c.g == 0.25f && c.b == 0.5f && c.a == 0.75f, "GetColor %.2f %.2f %.2f %.2f", c.r, c.g, c.b, c.a);
        brush->Release();
    }

    ID2D1Bitmap *b = nullptr;
    D2D1_BITMAP_PROPERTIES bp = { props.pixelFormat, 96.0f, 96.0f };
    hr = rt->CreateBitmap(D2D1_SIZE_U{ 32, 16 }, nullptr, 0, &bp, &b);
    CHECK(SUCCEEDED(hr), "a Direct2D bitmap (%#lx)", (unsigned long)hr);
    if (b)
    {
        D2D1_SIZE_F bs = b->GetSize();
        D2D1_SIZE_U bpx = b->GetPixelSize();
        CHECK(bs.width == 32.0f && bs.height == 16.0f && bpx.width == 32 && bpx.height == 16,
              "the bitmap's GetSize %.1fx%.1f, GetPixelSize %ux%u", bs.width, bs.height, bpx.width, bpx.height);
        b->Release();
    }

    rt->Release();
    d2d->Release();
    bmp->Release();
    wic->Release();
    printf("%d failures\n", failures);
    return failures ? 1 : 0;
}
