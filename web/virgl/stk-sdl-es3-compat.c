/*
 * SuperTuxKart asks SDL/GLX for an explicit OpenGL ES 3 context when its
 * shader renderer is enabled.  Mesa's virpipe path can provide ES 3.0, but
 * its GLX frontend rejects that explicit request; the otherwise identical
 * ES 2 request succeeds and returns the highest supported ES context (3.0).
 *
 * Keep this deliberately tiny and STK-specific: rewrite only the context
 * major attribute.  STK still validates the returned context with
 * glGetIntegerv(GL_MAJOR_VERSION) before enabling GLSL.
 */
#define _GNU_SOURCE
#include <dlfcn.h>

enum { SDL_GL_CONTEXT_MAJOR_VERSION = 17 };

int SDL_GL_SetAttribute(int attr, int value)
{
    static int (*real_set_attribute)(int, int);
    if (!real_set_attribute)
        real_set_attribute = dlsym(RTLD_NEXT, "SDL_GL_SetAttribute");
    if (attr == SDL_GL_CONTEXT_MAJOR_VERSION && value == 3)
        value = 2;
    return real_set_attribute(attr, value);
}

/*
 * STK sizes its shader texture-loader pool from this libstdc++ entry point.
 * gex currently cannot reliably park several guest workers on the same
 * condition variable, so expose one virtual CPU to this process.  STK adds
 * one loader of its own, leaving two workers without disabling async loads.
 */
unsigned gex_stk_hardware_concurrency(void)
    __asm__("_ZNSt6thread20hardware_concurrencyEv");

unsigned gex_stk_hardware_concurrency(void)
{
    return 1;
}
