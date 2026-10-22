#include <dinput.h>
#include <windows.h>
static volatile int key_messages;
static DIOBJECTDATAFORMAT key_objects[256];
static DIDATAFORMAT key_format = { sizeof(DIDATAFORMAT), sizeof(DIOBJECTDATAFORMAT), DIDF_RELAXIS, 256, 256, key_objects };
static LRESULT CALLBACK proc(HWND w, UINT msg, WPARAM wp, LPARAM lp) {
    if (msg == WM_KEYDOWN && wp == 'A') {
        key_messages++;
    }
    return DefWindowProcW(w, msg, wp, lp);
}
int entry(void) {
    WNDCLASSW cls = { 0 };
    cls.lpfnWndProc = proc;
    cls.hInstance = GetModuleHandleW(0);
    cls.lpszClassName = L"Input32";
    if (!RegisterClassW(&cls)) {
        return 1;
    }
    HWND w = CreateWindowExW(0, cls.lpszClassName, L"Input and font", WS_VISIBLE, 0, 0, 320, 200, 0, 0, cls.hInstance, 0);
    if (!w) {
        return 2;
    }
    HDC dc = GetDC(w);
    HFONT font = CreateFontW(-24, 0, 0, 0, 400, 0, 0, 0, 128, 0, 0, 0, 0, L"sans-serif");
    if (!font) {
        return 3;
    }
    SelectObject(dc, font);
    TEXTMETRICW metric;
    SIZE extent;
    if (!GetTextMetricsW(dc, &metric) || metric.tmHeight <= 0 || !GetTextExtentPoint32W(dc, L"Hello", 5, &extent) || extent.cx <= 0) {
        return 4;
    }
    MAT2 identity = { { 0, 1 }, { 0, 0 }, { 0, 0 }, { 0, 1 } };
    GLYPHMETRICS glyph;
    DWORD size = GetGlyphOutlineW(dc, 'A', GGO_GRAY8_BITMAP, &glyph, 0, 0, &identity);
    if (size == GDI_ERROR || !size || size > 4096) {
        return 5;
    }
    static unsigned char pixels[4096];
    if (GetGlyphOutlineW(dc, 'A', GGO_GRAY8_BITMAP, &glyph, sizeof(pixels), pixels, &identity) != size) {
        return 6;
    }
    unsigned sum = 0;
    for (unsigned i = 0; i < size; ++i) {
        sum += pixels[i];
    }
    if (!sum) {
        return 7;
    }
    OutputDebugStringA("READY: keyboard and glyph mask\n");
    IDirectInput8W *di = 0;
    IDirectInputDevice8W *keyboard = 0;
    for (unsigned i = 0; i < 256; ++i) {
        key_objects[i].pguid = &GUID_Key;
        key_objects[i].dwOfs = i;
        key_objects[i].dwType = DIDFT_BUTTON | DIDFT_MAKEINSTANCE(i);
    }
    if (FAILED(DirectInput8Create(cls.hInstance, 0x800, &IID_IDirectInput8W, (void **)&di, 0))) {
        return 8;
    }
    if (FAILED(di->lpVtbl->CreateDevice(di, &GUID_SysKeyboard, &keyboard, 0)) ||
        FAILED(keyboard->lpVtbl->SetDataFormat(keyboard, &key_format)) ||
        FAILED(keyboard->lpVtbl->SetCooperativeLevel(keyboard, w, DISCL_NONEXCLUSIVE | DISCL_FOREGROUND)) ||
        FAILED(keyboard->lpVtbl->Acquire(keyboard))) {
        return 9;
    }
    for (unsigned i = 0; i < 2000; ++i) {
        MSG msg;
        while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        unsigned char state[256], scan[256];
        if (!GetKeyboardState(state) || FAILED(keyboard->lpVtbl->GetDeviceState(keyboard, 256, scan))) {
            return 10;
        }
        if ((state['A'] & 128) && (GetAsyncKeyState('A') & 0x8000) && scan[DIK_A] && key_messages) {
            keyboard->lpVtbl->Unacquire(keyboard);
            keyboard->lpVtbl->Release(keyboard);
            di->lpVtbl->Release(di);
            ReleaseDC(w, dc);
            DestroyWindow(w);
            if (!DeleteObject(font) || !UnregisterClassW(cls.lpszClassName, cls.hInstance)) {
                return 11;
            }
            OutputDebugStringA("PASS: PE32 keyboard messages, DirectInput and browser glyphs\n");
            return 0;
        }
        Sleep(10);
    }
    return 12;
}
