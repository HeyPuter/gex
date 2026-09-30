/* GDI and painting without a display: memory DCs and bitmaps, PatBlt,
 * BitBlt and StretchBlt with ROPs, DIB sections and DIB transfers, pattern
 * brushes, regions and clipping; then a window's update region, WM_PAINT and
 * WM_ERASEBKGND, BeginPaint/EndPaint, window DCs drawing into the window's
 * surface, and CS_OWNDC.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld -municode -lgdi32 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static int paints, erases, paint_fErase;
static RECT paint_rc;
static COLORREF paint_color = RGB(10, 20, 30);

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    switch (m) {
    case WM_ERASEBKGND:
        erases++;
        break; /* DefWindowProc fills with the class brush */
    case WM_PAINT: {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(w, &ps);
        paints++;
        paint_rc = ps.rcPaint;
        paint_fErase = ps.fErase;
        HBRUSH b = CreateSolidBrush(paint_color);
        RECT r = {0, 0, 10, 10};
        FillRect(dc, &r, b);
        DeleteObject(b);
        EndPaint(w, &ps);
        return 0;
    }
    }
    return DefWindowProcW(w, m, wp, lp);
}

static void pump(void) {
    MSG msg;
    while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
}

int wmain(void) {
    /* Memory DCs, PatBlt, pixels. */
    HDC mem = CreateCompatibleDC(0);
    HBITMAP bm = CreateCompatibleBitmap(GetDC(0), 16, 16);
    HGDIOBJ old = SelectObject(mem, bm);
    CHECK("select into a memory DC returns the default bitmap", old != 0 && GetObjectType(old) == OBJ_BITMAP);
    SelectObject(mem, GetStockObject(WHITE_BRUSH));
    PatBlt(mem, 0, 0, 16, 16, PATCOPY);
    CHECK("PatBlt PATCOPY with the white brush", GetPixel(mem, 3, 3) == RGB(255, 255, 255));
    PatBlt(mem, 0, 0, 8, 8, BLACKNESS);
    CHECK("BLACKNESS", GetPixel(mem, 7, 7) == 0 && GetPixel(mem, 8, 8) == RGB(255, 255, 255));
    SetPixel(mem, 12, 1, RGB(1, 2, 3));
    CHECK("SetPixel/GetPixel", GetPixel(mem, 12, 1) == RGB(1, 2, 3));
    PatBlt(mem, 0, 0, 16, 16, DSTINVERT);
    CHECK("DSTINVERT", GetPixel(mem, 12, 1) == RGB(254, 253, 252) && GetPixel(mem, 0, 0) == RGB(255, 255, 255));
    CHECK("GetPixel outside the bitmap is CLR_INVALID", GetPixel(mem, 16, 0) == CLR_INVALID);

    /* BitBlt and StretchBlt between memory DCs. */
    HDC mem2 = CreateCompatibleDC(mem);
    HBITMAP bm2 = CreateCompatibleBitmap(mem, 32, 32);
    SelectObject(mem2, bm2);
    PatBlt(mem2, 0, 0, 32, 32, BLACKNESS);
    BitBlt(mem2, 4, 4, 16, 16, mem, 0, 0, SRCCOPY);
    CHECK("BitBlt SRCCOPY", GetPixel(mem2, 4 + 12, 4 + 1) == RGB(254, 253, 252) && GetPixel(mem2, 3, 3) == 0);
    StretchBlt(mem2, 0, 0, 32, 32, mem, 0, 0, 16, 16, SRCCOPY);
    CHECK("StretchBlt doubles", GetPixel(mem2, 25, 3) == RGB(254, 253, 252) && GetPixel(mem2, 0, 0) == RGB(255, 255, 255));
    BitBlt(mem2, 0, 0, 32, 32, 0, 0, 0, WHITENESS);
    BitBlt(mem2, 0, 0, 16, 16, mem, 0, 0, SRCAND);
    CHECK("SRCAND", GetPixel(mem2, 12, 1) == RGB(254, 253, 252) && GetPixel(mem2, 20, 20) == RGB(255, 255, 255));

    /* A DIB section: its bits are the pixels. */
    BITMAPINFO bi = {0};
    bi.bmiHeader.biSize = sizeof(bi.bmiHeader);
    bi.bmiHeader.biWidth = 8;
    bi.bmiHeader.biHeight = -8; /* top-down */
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    DWORD *bits = 0;
    HBITMAP dib = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, (void **)&bits, 0, 0);
    CHECK("CreateDIBSection gives bits", dib && bits);
    HDC mem3 = CreateCompatibleDC(mem);
    SelectObject(mem3, dib);
    SetPixel(mem3, 2, 1, RGB(0x11, 0x22, 0x33));
    CHECK("a DIB section's pixel is in its bits", (bits[1 * 8 + 2] & 0xffffff) == 0x112233);
    bits[0] = 0x00abcdef;
    CHECK("and writes to the bits show", GetPixel(mem3, 0, 0) == RGB(0xab, 0xcd, 0xef));
    DIBSECTION ds;
    CHECK("GetObject on a DIB section", GetObjectW(dib, sizeof(ds), &ds) == sizeof(ds) && ds.dsBm.bmWidth == 8 && ds.dsBmih.biBitCount == 32);
    DWORD out[64] = {0};
    BITMAPINFO gi = bi;
    gi.bmiHeader.biHeight = 8; /* bottom-up */
    CHECK("GetDIBits", GetDIBits(mem, dib, 0, 8, out, &gi, DIB_RGB_COLORS) == 8 && (out[7 * 8] & 0xffffff) == 0xabcdef);
    DWORD src[4] = {0x00ff0000, 0x0000ff00, 0x000000ff, 0x00ffffff}; /* 2x2, bottom-up */
    BITMAPINFO si = bi;
    si.bmiHeader.biWidth = 2;
    si.bmiHeader.biHeight = 2;
    StretchDIBits(mem3, 0, 0, 4, 4, 0, 0, 2, 2, src, &si, DIB_RGB_COLORS, SRCCOPY);
    CHECK("StretchDIBits bottom-up, stretched", GetPixel(mem3, 0, 0) == RGB(0, 0, 255) && GetPixel(mem3, 3, 3) == RGB(0, 255, 0) && GetPixel(mem3, 1, 3) == RGB(255, 0, 0));

    /* Pattern brushes. */
    WORD pat[8] = {0x55, 0xaa, 0x55, 0xaa, 0x55, 0xaa, 0x55, 0xaa};
    HBITMAP pbm = CreateBitmap(8, 8, 1, 1, pat);
    HBRUSH pb = CreatePatternBrush(pbm);
    SetTextColor(mem2, RGB(255, 0, 0));
    SetBkColor(mem2, RGB(0, 0, 255));
    RECT all = {0, 0, 8, 8};
    FillRect(mem2, &all, pb);
    COLORREF p0 = GetPixel(mem2, 0, 0), p1 = GetPixel(mem2, 1, 0);
    CHECK("a monochrome pattern brush uses the text and background colors", p0 != p1 && (p0 == RGB(255, 0, 0) || p0 == RGB(0, 0, 255)) && (p1 == RGB(255, 0, 0) || p1 == RGB(0, 0, 255)));

    /* Regions and clipping. */
    HRGN a = CreateRectRgn(0, 0, 10, 10), b = CreateRectRgn(5, 5, 20, 20), r = CreateRectRgn(0, 0, 0, 0);
    RECT box;
    CHECK("CombineRgn AND is simple", CombineRgn(r, a, b, RGN_AND) == SIMPLEREGION && GetRgnBox(r, &box) && box.left == 5 && box.right == 10);
    CHECK("CombineRgn OR is complex", CombineRgn(r, a, b, RGN_OR) == COMPLEXREGION && GetRgnBox(r, &box) && box.right == 20 && box.bottom == 20);
    CHECK("CombineRgn DIFF", CombineRgn(r, a, b, RGN_DIFF) == COMPLEXREGION && PtInRegion(r, 1, 1) && !PtInRegion(r, 7, 7));
    PatBlt(mem2, 0, 0, 32, 32, WHITENESS);
    SelectClipRgn(mem2, r);
    PatBlt(mem2, 0, 0, 32, 32, BLACKNESS);
    CHECK("drawing is clipped to the region", GetPixel(mem2, 1, 1) == 0 && GetPixel(mem2, 7, 7) == RGB(255, 255, 255) && GetPixel(mem2, 15, 15) == RGB(255, 255, 255));
    CHECK("GetClipBox", GetClipBox(mem2, &box) == COMPLEXREGION && box.left == 0 && box.right == 10);
    SelectClipRgn(mem2, 0);
    IntersectClipRect(mem2, 2, 2, 4, 4);
    PatBlt(mem2, 0, 0, 32, 32, BLACKNESS);
    CHECK("IntersectClipRect", GetPixel(mem2, 3, 3) == 0 && GetPixel(mem2, 20, 20) == RGB(255, 255, 255));
    SelectClipRgn(mem2, 0);
    int saved = SaveDC(mem2);
    SetViewportOrgEx(mem2, 10, 10, 0);
    SetPixel(mem2, 0, 0, RGB(9, 9, 9));
    RestoreDC(mem2, saved);
    CHECK("the viewport origin offsets drawing", GetPixel(mem2, 10, 10) == RGB(9, 9, 9));

    /* A window: painting through its update region. */
    WNDCLASSW wc = {0};
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gex_gdi";
    wc.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    RegisterClassW(&wc);
    HWND w = CreateWindowExW(0, L"gex_gdi", L"gdi", WS_POPUP, 50, 50, 100, 80, 0, 0, wc.hInstance, 0);
    pump();
    CHECK("no WM_PAINT for a hidden window", paints == 0);
    ShowWindow(w, SW_SHOW);
    UpdateWindow(w);
    CHECK("UpdateWindow paints once", paints == 1);
    CHECK("the whole client area, erased first", paint_rc.right == 100 && paint_rc.bottom == 80 && erases == 1 && !paint_fErase);
    HDC wdc = GetDC(w);
    CHECK("the paint went to the window", GetPixel(wdc, 5, 5) == paint_color);
    CHECK("and the background is the class brush", GetPixel(wdc, 50, 50) == GetSysColor(COLOR_WINDOW));
    CHECK("GetClipBox of a window DC is its client area", GetClipBox(wdc, &box) == SIMPLEREGION && box.right == 100 && box.bottom == 80);
    ReleaseDC(w, wdc);
    RECT inv = {20, 20, 30, 40}, upd;
    InvalidateRect(w, &inv, FALSE);
    CHECK("GetUpdateRect", GetUpdateRect(w, &upd, FALSE) && upd.left == 20 && upd.bottom == 40);
    MSG msg;
    CHECK("PeekMessage finds WM_PAINT", PeekMessageW(&msg, 0, WM_PAINT, WM_PAINT, PM_NOREMOVE) && msg.message == WM_PAINT && msg.hwnd == w);
    paint_color = RGB(40, 50, 60);
    pump();
    CHECK("dispatching it paints just that part", paints == 2 && paint_rc.left == 20 && paint_rc.top == 20 && erases == 1);
    wdc = GetDC(w);
    CHECK("drawing was clipped to the update region", GetPixel(wdc, 5, 5) == RGB(10, 20, 30));
    ReleaseDC(w, wdc);
    CHECK("then nothing is left to paint", !GetUpdateRect(w, 0, FALSE) && !PeekMessageW(&msg, 0, WM_PAINT, WM_PAINT, PM_NOREMOVE));
    InvalidateRect(w, 0, TRUE);
    ValidateRect(w, 0);
    CHECK("ValidateRect", !GetUpdateRect(w, 0, FALSE));
    DestroyWindow(w);

    /* CS_OWNDC. */
    wc.lpszClassName = L"gex_owndc";
    wc.style = CS_OWNDC;
    RegisterClassW(&wc);
    HWND ow = CreateWindowExW(0, L"gex_owndc", L"own", WS_POPUP, 0, 0, 20, 20, 0, 0, wc.hInstance, 0);
    HDC o1 = GetDC(ow);
    ReleaseDC(ow, o1);
    HDC o2 = GetDC(ow);
    CHECK("a CS_OWNDC window keeps its DC", o1 == o2);
    SetTextColor(o2, RGB(1, 2, 3));
    ReleaseDC(ow, o2);
    CHECK("and its attributes", GetTextColor(GetDC(ow)) == RGB(1, 2, 3));
    DestroyWindow(ow);

    DeleteDC(mem3);
    DeleteDC(mem2);
    DeleteDC(mem);
    printf("%d failures\n", failures);
    return failures;
}
