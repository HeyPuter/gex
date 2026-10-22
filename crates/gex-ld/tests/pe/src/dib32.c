#include <windows.h>
int entry(void) {
    HDC dc = CreateCompatibleDC(0);
    HFONT font = CreateFontW(-24, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, 0, 0, 0, 0, L"sans-serif");
    if (!dc || !font) {
        return 1;
    }
    SelectObject(dc, font);
    for (int mode = 0; mode < 4; mode++) {
        int top = mode & 1;
        BITMAPV4HEADER info = { 0 };
        info.bV4Size = mode < 2 ? 40 : sizeof(info);
        info.bV4Width = 64;
        info.bV4Height = top ? -48 : 48;
        info.bV4Planes = 1;
        info.bV4BitCount = 32;
        if (mode >= 2) {
            info.bV4V4Compression = BI_BITFIELDS;
            info.bV4RedMask = 0xff0000;
            info.bV4GreenMask = 0xff00;
            info.bV4BlueMask = 0xff;
            info.bV4AlphaMask = 0xff000000;
        }
        unsigned char *data = 0;
        HBITMAP bitmap = CreateDIBSection(dc, (BITMAPINFO *)&info, DIB_RGB_COLORS, (void **)&data, 0, 0);
        if (!bitmap || !data) {
            return 2;
        }
        HGDIOBJ old = SelectObject(dc, bitmap);
        if (!old || DeleteObject(bitmap)) {
            return 3;
        }
        if (SetBkMode(dc, TRANSPARENT) != OPAQUE || SetTextColor(dc, RGB(255, 0, 0)) != RGB(0, 0, 0)) {
            return 4;
        }
        if (!TextOutW(dc, 2, 2, L"A", 1)) {
            return 5;
        }
        unsigned red = 0;
        for (unsigned y = 0; y < 48; y++) {
            for (unsigned x = 0; x < 64; x++) {
                unsigned char *p = data + ((top ? y : 47 - y) * 64 + x) * 4;
                if (p[0] || p[1] || p[3]) {
                    return 6;
                }
                if (p[2]) {
                    red++;
                    if (y > 30) {
                        return 7;
                    }
                }
            }
        }
        if (red < 20) {
            return 8;
        }
        HBRUSH brush = CreateSolidBrush(RGB(0, 0, 255));
        RECT rect = { 0, 0, 1, 1 };
        if (!brush || !FillRect(dc, &rect, brush)) {
            return 10;
        }
        unsigned char *corner = data + (top ? 0 : 47) * 64 * 4;
        if (corner[0] != 255 || corner[1] || corner[2] || !DeleteObject(brush)) {
            return 11;
        }
        if (SelectObject(dc, old) != bitmap || !DeleteObject(bitmap)) {
            return 8;
        }
        SetBkMode(dc, OPAQUE);
        SetTextColor(dc, 0);
    }
    if (!DeleteDC(dc) || !DeleteObject(font)) {
        return 9;
    }
    OutputDebugStringA("PASS: DIB text pixels, BGR color, row orientation and selected-object lifetime\n");
    return 0;
}
