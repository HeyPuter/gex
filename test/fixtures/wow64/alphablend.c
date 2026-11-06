/* test/wow64/alphablend: msimg32's AlphaBlend (a forward to gdi32's
 * GdiAlphaBlend) and GdiAlphaBlend, called directly and through
 * GetProcAddress, blend an opaque white source over black: with a constant
 * alpha of 255 the pixel is white, with 128 mid grey, and with the source's
 * own alpha (AC_SRC_ALPHA) white again. BLENDFUNCTION is a 4-byte struct
 * passed by value, which wasm takes by reference. Prints "alphablend ok". */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

typedef BOOL (WINAPI *blend_fn)(HDC, int, int, int, int, HDC, int, int, int, int, BLENDFUNCTION);

static DWORD *dib(HDC dc, HBITMAP *bmp, DWORD fill)
{
    BITMAPINFO bi = { { sizeof(bi.bmiHeader), 4, -4, 1, 32, BI_RGB } };
    DWORD *p;
    int i;
    *bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, (void **)&p, NULL, 0);
    SelectObject(dc, *bmp);
    for (i = 0; i < 16; i++) p[i] = fill;
    return p;
}

/* blend with `alpha` and `format`; the green channel must be `want` (+-2) */
static int check(const char *what, blend_fn fn, BYTE alpha, BYTE format, int want)
{
    HDC dst = CreateCompatibleDC(0), src = CreateCompatibleDC(0);
    HBITMAP bd, bs;
    DWORD *d = dib(dst, &bd, 0xff000000);
    BLENDFUNCTION bf = { AC_SRC_OVER, 0, alpha, format };
    int g, ok;
    dib(src, &bs, 0xffffffff);
    if (!fn(dst, 0, 0, 4, 4, src, 0, 0, 4, 4, bf)) {
        printf("%s %u/%u: failed (%lu)\n", what, alpha, format, GetLastError());
        return 0;
    }
    GdiFlush();
    g = (d[5] >> 8) & 0xff;
    ok = abs(g - want) <= 2;
    if (!ok) printf("%s %u/%u: pixel %08lx, want green %d\n", what, alpha, format, d[5], want);
    DeleteDC(dst); DeleteDC(src); DeleteObject(bd); DeleteObject(bs);
    return ok;
}

int main(void)
{
    struct { const char *name; blend_fn fn; } fns[] = {
        { "AlphaBlend", AlphaBlend },
        { "GdiAlphaBlend", GdiAlphaBlend },
        { "msimg32!AlphaBlend", (blend_fn)GetProcAddress(LoadLibraryA("msimg32.dll"), "AlphaBlend") },
        { "gdi32!GdiAlphaBlend", (blend_fn)GetProcAddress(GetModuleHandleA("gdi32.dll"), "GdiAlphaBlend") },
    };
    int ok = 1, i;
    for (i = 0; i < 4; i++) {
        ok &= check(fns[i].name, fns[i].fn, 255, 0, 255);
        ok &= check(fns[i].name, fns[i].fn, 128, 0, 128);
        ok &= check(fns[i].name, fns[i].fn, 255, AC_SRC_ALPHA, 255);
    }
    if (ok) printf("alphablend ok\n");
    return !ok;
}
