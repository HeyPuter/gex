/* OpenGL on a window of Wine's null display driver, which has no GL surfaces
 * of its own: win32u draws it into framebuffer objects (its framebuffer
 * surface), which take GL 4.5's glCreateFramebuffers. Without them the pixel
 * format fails to set, and so does a context, never a crash; with them, a
 * clear read back, the window resized (the surface's buffers too) and
 * cleared and read again, then the context and window gone.
 * Prints "ok ..." lines and "0 failures".
 * gex-nt-arch: x64
 * gex-nt-libs: -lopengl32 -lgdi32
 */
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>

static int failures;
#define CHECK(c, ...) do { if (c) printf("ok "); else { printf("FAIL "); failures++; } printf(__VA_ARGS__); printf("\n"); } while (0)

static int read_pixel(int x, int y, unsigned char want_r, unsigned char want_g, unsigned char want_b)
{
    unsigned char px[4] = {0};
    glReadPixels(x, y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    printf("  pixel %d,%d: %u %u %u\n", x, y, px[0], px[1], px[2]);
    return px[0] == want_r && px[1] == want_g && px[2] == want_b;
}

int main(void)
{
    PIXELFORMATDESCRIPTOR pfd = { sizeof(pfd), 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                                  PFD_TYPE_RGBA, 32, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 24, 8 };
    HWND hwnd;
    HDC hdc;
    HGLRC ctx;
    int fmt;

    setvbuf(stdout, NULL, _IONBF, 0);
    hwnd = CreateWindowA("static", "glnulldrv", WS_OVERLAPPEDWINDOW, 0, 0, 160, 120, NULL, NULL, NULL, NULL);
    hdc = GetDC(hwnd);
    fmt = ChoosePixelFormat(hdc, &pfd);
    CHECK(fmt > 0, "a pixel format (%d)", fmt);
    if (!SetPixelFormat(hdc, fmt, &pfd))
    {
        printf("ok SetPixelFormat on a null driver window failed (error %lu): no framebuffer surface here\n", GetLastError());
        ctx = wglCreateContext(hdc);
        CHECK(!ctx, "and so did a context");
        if (ctx) wglDeleteContext(ctx);
        ReleaseDC(hwnd, hdc);
        DestroyWindow(hwnd);
        printf("%d failures\n", failures);
        return failures ? 1 : 0;
    }
    printf("ok SetPixelFormat on a null driver window\n");
    ctx = wglCreateContext(hdc);
    CHECK(ctx != NULL, "a context");
    if (!ctx) return 1;
    CHECK(wglMakeCurrent(hdc, ctx), "made current");

    /* (a flush first: the surface takes the window's size at the first one) */
    glFinish();
    glViewport(0, 0, 160, 120);
    glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    CHECK(read_pixel(80, 60, 255, 0, 0), "the clear reads back red");
    CHECK(glGetError() == GL_NO_ERROR, "no GL error");
    CHECK(SwapBuffers(hdc), "SwapBuffers");

    /* larger: the surface's renderbuffers resized */
    SetWindowPos(hwnd, NULL, 0, 0, 320, 240, SWP_NOMOVE | SWP_NOZORDER);
    glViewport(0, 0, 300, 200);
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();
    CHECK(read_pixel(250, 150, 0, 255, 0), "after a resize, a clear reads back green outside the old size");
    CHECK(SwapBuffers(hdc), "SwapBuffers again");

    wglMakeCurrent(NULL, NULL);
    CHECK(wglDeleteContext(ctx), "the context deleted");
    ReleaseDC(hwnd, hdc);
    DestroyWindow(hwnd);
    printf("%d failures\n", failures);
    return failures ? 1 : 0;
}
