/* window.c in OpenGL: a captioned window whose 320x240 client area is drawn
 * with wgl and glClear -- four coloured quadrants and a white square in the
 * middle -- and SwapBuffers in a loop. A click prints "click X Y" and leaves
 * a black mark there; a key prints "key VK"; Escape or closing the window
 * ends it, as does the argument's number of seconds (default 30). Prints
 * "shown" after the first frames and "frames N" and "bye" at the end. What
 * the page's direct GL frame (and the readback fallback) is checked against.
 * The seconds end it no sooner than its first frames (a start slowed by a
 * busy host still prints "shown"), and a minute past them it ends anyway.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld -lgdi32 -lopengl32 */
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>
#include <stdlib.h>

static POINT marks[64];
static int nmarks, done, frames, due;

static void quad(int l, int t, int r, int b, int H, float R, float G, float B) {
    glScissor(l, H - b, r - l, b - t);
    glClearColor(R, G, B, 1);
    glClear(GL_COLOR_BUFFER_BIT);
}

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    switch (m) {
    case WM_LBUTTONDOWN: {
        int x = (short)LOWORD(lp), y = (short)HIWORD(lp);
        printf("click %d %d\n", x, y);
        fflush(stdout);
        if (nmarks < 64) marks[nmarks++] = (POINT){x, y};
        return 0;
    }
    case WM_KEYDOWN:
        printf("key %u\n", (unsigned)wp);
        fflush(stdout);
        if (wp == VK_ESCAPE) DestroyWindow(w);
        return 0;
    case WM_TIMER:
        /* timer 1: the seconds are up, once the first frames are drawn;
         * timer 2: a minute past them, drawn or not */
        if (wp == 1 && frames < 3) { due = 1; KillTimer(w, 1); return 0; }
        DestroyWindow(w);
        return 0;
    case WM_DESTROY:
        done = 1;
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(w, m, wp, lp);
}

int main(int argc, char **argv) {
    int secs = argc > 1 ? atoi(argv[1]) : 30;
    WNDCLASSW wc = {0};
    wc.style = CS_OWNDC;
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.hCursor = LoadCursorW(0, (LPCWSTR)IDC_ARROW);
    wc.lpszClassName = L"gex_glwindow";
    RegisterClassW(&wc);
    RECT r = {0, 0, 320, 240};
    AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
    HWND w = CreateWindowExW(0, L"gex_glwindow", L"gex gl window", WS_OVERLAPPEDWINDOW, 100, 100,
                             r.right - r.left, r.bottom - r.top, 0, 0, wc.hInstance, 0);
    HDC dc = GetDC(w);
    PIXELFORMATDESCRIPTOR pfd = {sizeof pfd, 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                                 PFD_TYPE_RGBA, 32};
    pfd.cDepthBits = 24;
    SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd);
    HGLRC gl = wglCreateContext(dc);
    wglMakeCurrent(dc, gl);
    ShowWindow(w, SW_SHOW);
    SetTimer(w, 1, secs * 1000, 0);
    SetTimer(w, 2, secs * 1000 + 60000, 0);
    while (!done) {
        MSG msg;
        while (PeekMessageW(&msg, 0, 0, 0, PM_REMOVE)) {
            if (msg.message == WM_QUIT) done = 1;
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if (done) break;
        RECT rc;
        GetClientRect(w, &rc);
        int W = rc.right, H = rc.bottom, cx = W / 2, cy = H / 2;
        glViewport(0, 0, W, H);
        glEnable(GL_SCISSOR_TEST);
        quad(0, 0, cx, cy, H, 220 / 255.f, 40 / 255.f, 40 / 255.f);
        quad(cx, 0, W, cy, H, 40 / 255.f, 200 / 255.f, 60 / 255.f);
        quad(0, cy, cx, H, H, 40 / 255.f, 80 / 255.f, 220 / 255.f);
        quad(cx, cy, W, H, H, 230 / 255.f, 210 / 255.f, 40 / 255.f);
        quad(cx - 20, cy - 20, cx + 20, cy + 20, H, 1, 1, 1);
        for (int i = 0; i < nmarks; i++)
            quad(marks[i].x - 4, marks[i].y - 4, marks[i].x + 4, marks[i].y + 4, H, 0, 0, 0);
        glDisable(GL_SCISSOR_TEST);
        SwapBuffers(dc);
        if (++frames == 3) {
            printf("shown\n");
            fflush(stdout);
            if (due) DestroyWindow(w);
        }
    }
    printf("frames %d\nbye\n", frames);
    wglMakeCurrent(0, 0);
    wglDeleteContext(gl);
    return 0;
}
