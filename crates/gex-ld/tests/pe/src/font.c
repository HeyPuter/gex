#include <windows.h>
static const WCHAR *volatile family = L"sans-serif";
int entry(void) {
    HDC dc = CreateCompatibleDC(0);
    if (!dc) {
        return 1;
    }
    HFONT font = CreateFontW(-24, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, 0, 0, 0, 0, family);
    if (!font) {
        return 2;
    }
    LOGFONTW lf;
    if (GetObjectW(font, sizeof(lf), &lf) != sizeof(lf) || lf.lfHeight != -24 || lf.lfWeight != 400) {
        return 3;
    }
    HGDIOBJ old = SelectObject(dc, font);
    if (!old || DeleteObject(font)) {
        return 4;
    }
    TEXTMETRICW tm;
    if (!GetTextMetricsW(dc, &tm) || tm.tmHeight <= 0 || tm.tmAscent <= 0) {
        return 5;
    }
    SIZE narrow, wide;
    if (!GetTextExtentPoint32W(dc, L"iii", 3, &narrow) || !GetTextExtentPoint32W(dc, L"WWW", 3, &wide) || wide.cx <= narrow.cx) {
        return 6;
    }
    MAT2 transform = { { 0, 1 }, { 0, 0 }, { 0, 0 }, { 0, 1 } };
    GLYPHMETRICS gm;
    DWORD size = GetGlyphOutlineW(dc, 'A', GGO_GRAY8_BITMAP, &gm, 0, 0, &transform);
    if (!size || size == GDI_ERROR || !gm.gmBlackBoxX || !gm.gmBlackBoxY) {
        return 7;
    }
    BYTE *pixels = HeapAlloc(GetProcessHeap(), 0, size);
    if (!pixels) {
        return 8;
    }
    if (GetGlyphOutlineW(dc, 'A', GGO_GRAY8_BITMAP, &gm, size, pixels, &transform) != size) {
        return 9;
    }
    unsigned ink = 0;
    for (DWORD i = 0; i < size; ++i) {
        if (pixels[i] > 64) {
            return 10;
        }
        ink += pixels[i];
    }
    if (!ink) {
        return 11;
    }
    if (GetGlyphOutlineW(dc, 'A', GGO_GRAY8_BITMAP, &gm, size - 1, pixels, &transform) != GDI_ERROR) {
        return 12;
    }
    HeapFree(GetProcessHeap(), 0, pixels);
    BITMAPINFO info = { 0 };
    info.bmiHeader.biSize = 40;
    info.bmiHeader.biWidth = 2;
    info.bmiHeader.biHeight = 2;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 24;
    BYTE dib[16] = { 255, 0, 0, 255, 255, 255, 0, 0, 0, 0, 255, 0, 255, 0, 0, 0 };
    HDC screen = GetDC(0);
    if (SetDIBitsToDevice(screen, 0, 0, 2, 2, 0, 0, 0, 2, dib, &info, DIB_RGB_COLORS) != 2 || !ReleaseDC(0, screen)) {
        return 14;
    }
    if (SelectObject(dc, old) != font || !DeleteObject(font) || !DeleteDC(dc)) {
        return 13;
    }
    return 0;
}
