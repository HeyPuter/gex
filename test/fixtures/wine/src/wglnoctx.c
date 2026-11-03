/* gex-nt-libs: -lopengl32 -lgdi32 -ld3d11 */
/* OpenGL and Direct3D calls that must fail or succeed, never crash, when the
 * thread has never had a context current: a context deleted by another
 * thread (wglDeleteContext then makes a dummy context current, through the
 * thread's empty function table), and a Direct3D device asked for where GL
 * may not work at all (wined3d's caps context: create, a make current that
 * fails, delete). Prints "ok ..." lines and "0 failures". */
#define COBJMACROS
#include <windows.h>
#include <d3d11.h>
#include <stdio.h>

static int failures;
#define CHECK(c, ...) do { if (c) printf("ok "); else { printf("FAIL "); failures++; } printf(__VA_ARGS__); printf("\n"); } while (0)

static DWORD WINAPI delete_thread(void *ctx)
{
    /* this thread has made no context current */
    return wglDeleteContext(ctx) ? 1 : 2;
}

static DWORD WINAPI release_thread(void *unused)
{
    /* nothing current: a release that does nothing */
    wglMakeCurrent(NULL, NULL);
    return 1;
}

int main(void)
{
    PIXELFORMATDESCRIPTOR pfd = { sizeof(pfd), 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                                  PFD_TYPE_RGBA, 32 };
    HWND hwnd = CreateWindowA("static", "wglnoctx", WS_POPUP, 0, 0, 64, 64, NULL, NULL, NULL, NULL);
    HDC hdc = GetDC(hwnd);
    int fmt = ChoosePixelFormat(hdc, &pfd);
    HGLRC ctx = NULL;
    DWORD code = 0;
    HANDLE th;
    ID3D11Device *dev = NULL;
    HRESULT hr;

    if (fmt && SetPixelFormat(hdc, fmt, &pfd)) ctx = wglCreateContext(hdc);
    if (ctx)
    {
        th = CreateThread(NULL, 0, delete_thread, ctx, 0, NULL);
        WaitForSingleObject(th, INFINITE);
        GetExitCodeThread(th, &code);
        CloseHandle(th);
        CHECK(code == 1, "wglDeleteContext from a thread with no context current");
    }
    else printf("ok no GL context here (format %d), the delete case is skipped\n", fmt);

    th = CreateThread(NULL, 0, release_thread, NULL, 0, NULL);
    WaitForSingleObject(th, INFINITE);
    GetExitCodeThread(th, &code);
    CloseHandle(th);
    CHECK(code == 1, "wglMakeCurrent(NULL, NULL) with nothing current");

    hr = D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0, D3D11_SDK_VERSION, &dev, NULL, NULL);
    CHECK(1, "D3D11CreateDevice returned %#lx", (unsigned long)hr);
    if (dev) ID3D11Device_Release(dev);

    ReleaseDC(hwnd, hdc);
    DestroyWindow(hwnd);
    printf("%d failures\n", failures);
    return failures ? 1 : 0;
}
