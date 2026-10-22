#include <windows.h>
static int down, up, character;
static LRESULT CALLBACK procedure(HWND w, UINT message, WPARAM key, LPARAM lp) {
    if (message == WM_KEYDOWN && key == 'A') {
        BYTE state[256];
        if (!GetKeyboardState(state) || !(state['A'] & 128) || !(GetKeyState('A') & 0x8000)) {
            ExitProcess(2);
        }
        down = 1;
    }
    if (message == WM_KEYUP && key == 'A') {
        if (GetKeyState('A') & 0x8000) {
            ExitProcess(3);
        }
        up = 1;
    }
    if (message == WM_CHAR && key == 'a') {
        character = 1;
    }
    return DefWindowProcW(w, message, key, lp);
}
static WNDPROC volatile callback = procedure;
int entry(void) {
    WNDCLASSW cls = { 0 };
    cls.lpfnWndProc = callback;
    cls.lpszClassName = L"Keyboard";
    if (!RegisterClassW(&cls) || !CreateWindowExW(0, L"Keyboard", L"Keyboard", 0, 0, 0, 320, 240, 0, 0, 0, 0)) {
        return 1;
    }
    for (;;) {
        MSG message;
        while (PeekMessageW(&message, 0, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if (down && up && character) {
            return 0;
        }
        Sleep(10);
    }
}
