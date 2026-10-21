#include <GL/gl.h>
#include <windows.h>
static const char *volatile class_name = "STATIC";
int entry(void) {
    HWND w = CreateWindowExA(0, class_name, "orientation", 0, 0, 0, 8, 8, 0, 0, 0, 0);
    HDC dc = GetDC(w);
    PIXELFORMATDESCRIPTOR p = { 0 };
    p.nSize = sizeof(p);
    p.nVersion = 1;
    p.dwFlags = PFD_SUPPORT_OPENGL | PFD_DRAW_TO_WINDOW | PFD_DOUBLEBUFFER;
    p.iPixelType = PFD_TYPE_RGBA;
    if (!w || !dc || !SetPixelFormat(dc, ChoosePixelFormat(dc, &p), &p)) {
        return 1;
    }
    HGLRC context = wglCreateContext(dc);
    if (!context || !wglMakeCurrent(dc, context)) {
        return 2;
    }
    HMODULE gl = LoadLibraryA("opengl32.dll");
    void(WINAPI * clear_color)(float, float, float, float) = (void *)GetProcAddress(gl, "glClearColor");
    void(WINAPI * clear)(unsigned) = (void *)GetProcAddress(gl, "glClear");
    void(WINAPI * scissor)(int, int, int, int) = (void *)GetProcAddress(gl, "glScissor");
    void(WINAPI * draw_buffer)(unsigned) = (void *)GetProcAddress(gl, "glDrawBuffer");
    void(WINAPI * read_buffer)(unsigned) = (void *)GetProcAddress(gl, "glReadBuffer");
    void(WINAPI * read_pixels)(int, int, int, int, unsigned, unsigned, void *) = (void *)GetProcAddress(gl, "glReadPixels");
    void(WINAPI * flush)(void) = (void *)GetProcAddress(gl, "glFlush");
    BOOL(WINAPI * swap)(HDC) = (void *)wglGetProcAddress("wglSwapBuffers");
    if (!clear_color || !clear || !scissor || !draw_buffer || !read_buffer || !read_pixels || !flush || !swap) {
        return 3;
    }
    for (unsigned i = 0; i < 3; ++i) {
        float alpha = i * .5f;
        draw_buffer(GL_BACK);
        glDisable(GL_SCISSOR_TEST);
        clear_color(1, 0, 0, alpha);
        clear(GL_COLOR_BUFFER_BIT);
        glEnable(GL_SCISSOR_TEST);
        scissor(0, 0, 8, 4);
        clear_color(0, 0, 1, alpha);
        clear(GL_COLOR_BUFFER_BIT);
        glDisable(GL_SCISSOR_TEST);
        /* Framebuffer alpha remains available to GL, but must not make the
         * ordinary window transparent to the browser page at presentation. */
        unsigned char pixel[4];
        read_buffer(GL_BACK);
        read_pixels(1, 1, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
        if (pixel[0] || pixel[1] || pixel[2] != 255 || pixel[3] != (i == 0 ? 0 : i == 1 ? 128 : 255)) {
            return 6;
        }
        if (!swap(dc)) {
            return 4;
        }
        /* Same pattern drawn directly to front: exercises drawable.flush_front. */
        draw_buffer(GL_FRONT);
        clear_color(1, 0, 0, alpha);
        clear(GL_COLOR_BUFFER_BIT);
        glEnable(GL_SCISSOR_TEST);
        scissor(0, 0, 8, 4);
        clear_color(0, 0, 1, alpha);
        clear(GL_COLOR_BUFFER_BIT);
        glDisable(GL_SCISSOR_TEST);
        flush();
    }
    if (glGetError()) {
        return 5;
    }
    wglMakeCurrent(0, 0);
    wglDeleteContext(context);
    ReleaseDC(w, dc);
    DestroyWindow(w);
    OutputDebugStringA("PASS: alternating swap and front flush with framebuffer alpha 0, 128, 255\n");
    return 0;
}
