/* A window taller than the screen and past its bottom right corner, with
 * a GDI strip along its top and an OpenGL child below it: the strip must
 * stay shown while the GL child presents. The popup is 600 wide and the
 * virtual screen's height plus 100 tall, 300x200 of it on the screen (its
 * top left at the screen's corner less that); it holds a red 600x26 strip
 * at the top (a child window, painted once) and a 590x370 GL child at
 * (5,26), cleared green each frame. Being taller than the screen, its
 * window surface is cropped to the screen, smaller than the window; the
 * window's buffer still has the strip's 300x26 red after the GL child's
 * frames. Prints "shown", "frames N" and "bye"; ends after the argument's
 * seconds (default 3).
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld -lgdi32 -lopengl32 */
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>
#include <stdlib.h>

static int done;

static LRESULT CALLBACK top_proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    if (m == WM_TIMER) { DestroyWindow(w); return 0; }
    if (m == WM_DESTROY) { done = 1; PostQuitMessage(0); return 0; }
    return DefWindowProcW(w, m, wp, lp);
}

static LRESULT CALLBACK strip_proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    if (m == WM_PAINT) {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(w, &ps);
        HBRUSH red = CreateSolidBrush(RGB(200, 30, 30));
        RECT rc;
        GetClientRect(w, &rc);
        FillRect(dc, &rc, red);
        DeleteObject(red);
        EndPaint(w, &ps);
        return 0;
    }
    return DefWindowProcW(w, m, wp, lp);
}

int main(int argc, char **argv) {
    int secs = argc > 1 ? atoi(argv[1]) : 3;
    HINSTANCE hi = GetModuleHandleW(0);
    WNDCLASSW wc = {0};
    wc.lpfnWndProc = top_proc;
    wc.hInstance = hi;
    wc.hbrBackground = GetStockObject(WHITE_BRUSH);
    wc.lpszClassName = L"gex_glbig";
    RegisterClassW(&wc);
    wc.style = CS_OWNDC;
    wc.lpfnWndProc = DefWindowProcW;
    wc.hbrBackground = 0;
    wc.lpszClassName = L"gex_glbig_gl";
    RegisterClassW(&wc);
    wc.style = 0;
    wc.lpfnWndProc = strip_proc;
    wc.lpszClassName = L"gex_glbig_strip";
    RegisterClassW(&wc);

    int h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    int x = GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN) - 300;
    int y = GetSystemMetrics(SM_YVIRTUALSCREEN) + h - 200;
    HWND top = CreateWindowExW(0, L"gex_glbig", L"gex gl big", WS_POPUP | WS_CLIPCHILDREN,
                               x, y, 600, h + 100, 0, 0, hi, 0);
    CreateWindowExW(0, L"gex_glbig_strip", L"", WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                    0, 0, 600, 26, top, 0, hi, 0);
    HWND glw = CreateWindowExW(0, L"gex_glbig_gl", L"", WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                               5, 26, 590, 370, top, 0, hi, 0);
    HDC dc = GetDC(glw);
    PIXELFORMATDESCRIPTOR pfd = {sizeof pfd, 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                                 PFD_TYPE_RGBA, 32};
    SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd);
    HGLRC gl = wglCreateContext(dc);
    wglMakeCurrent(dc, gl);
    ShowWindow(top, SW_SHOW);
    UpdateWindow(top);
    SetTimer(top, 1, secs * 1000, 0);
    int frames = 0;
    while (!done) {
        MSG msg;
        while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) {
            if (msg.message == WM_QUIT) done = 1;
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if (done) break;
        glViewport(0, 0, 590, 370);
        glClearColor(40 / 255.f, 190 / 255.f, 60 / 255.f, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        SwapBuffers(dc);
        if (++frames == 3) {
            printf("shown\n");
            fflush(stdout);
        }
        Sleep(10);
    }
    printf("frames %d\nbye\n", frames);
    wglMakeCurrent(0, 0);
    wglDeleteContext(gl);
    return 0;
}
