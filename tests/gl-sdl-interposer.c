#include <stdint.h>
#include <stdio.h>

typedef void *SDL_GLContext;

SDL_GLContext SDL_GL_CreateContext(void *window);

static unsigned bound_api;
static int initialized_after_bind;

void *eglGetDisplay(void *native)
{
    (void)native;
    return (void *)(uintptr_t)1;
}

unsigned eglBindAPI(unsigned api)
{
    bound_api = api;
    return 1;
}

unsigned eglInitialize(void *display, int *major, int *minor)
{
    (void)display;
    (void)major;
    (void)minor;
    initialized_after_bind = bound_api == 0x30a2u; /* EGL_OPENGL_API */
    return initialized_after_bind;
}

void *eglCreateContext(void *display, void *config, void *share,
                       const int *attributes)
{
    (void)display;
    (void)config;
    (void)share;
    (void)attributes;
    return initialized_after_bind ? (void *)(uintptr_t)3 : NULL;
}

unsigned eglMakeCurrent(void *display, void *draw, void *read, void *context)
{
    (void)display;
    (void)draw;
    (void)read;
    return context != NULL;
}

unsigned eglSwapBuffers(void *display, void *surface)
{
    (void)display;
    (void)surface;
    return 1;
}

void *eglGetProcAddress(const char *name)
{
    (void)name;
    return NULL;
}

int main(void)
{
    if (!SDL_GL_CreateContext(NULL)) {
        fputs("SDL_GL_CreateContext failed\n", stderr);
        return 1;
    }
    if (bound_api != 0x30a2u || !initialized_after_bind) {
        fprintf(stderr, "SDL interposer bound EGL API %#x\n", bound_api);
        return 1;
    }
    puts("gl-sdl-interposer-desktop-api");
    return 0;
}
