/* Do the pixels reach the window?
 *
 * Every other probe here renders into an FBO or a pbuffer and reads back with
 * glReadPixels, which never leaves the driver. Nothing tested the last step --
 * the driver handing a frame to the winsys and the winsys putting it on an X
 * drawable -- and that step was silently broken for months in a way no GL
 * call could report.
 *
 * The driver advertised no opaque (alpha 0) GLX config, because the
 * B8G8R8X8/R8G8B8X8 formats Mesa builds those from were unmapped. A toolkit
 * asking for an opaque window therefore chose between alpha configs, 24 of
 * which carried a *depth 32* visual, and drisw takes the XImage depth from
 * the chosen config's visual. Presenting to a depth-24 window then put a
 * depth-32 image into it: BadMatch, which X reports asynchronously, so
 * XShmPutImage returned success and drew nothing. Krita presented six frames
 * of its complete interface into a window that stayed pure black.
 *
 * So: create a window from the config's own visual, clear it to a known
 * colour, swap, and read the window back through X rather than through GL.
 * The opaque case is the one that was missing; the alpha case is here because
 * an implementation could plausibly break the other one instead.
 */
#include <GL/glx.h>
#include <GL/gl.h>
#include <X11/Xlib.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 64

/* The colour cleared and expected back, as bytes. */
#define WANT_R 64
#define WANT_G 128
#define WANT_B 191

/* want_depth 0 means "whatever the first matching config offers". */
static int run_case(Display *dpy, const char *name, int alpha_bits,
                    int want_depth)
{
    int attributes[] = {
        GLX_RENDER_TYPE, GLX_RGBA_BIT,
        GLX_DRAWABLE_TYPE, GLX_WINDOW_BIT,
        GLX_RED_SIZE, 8, GLX_GREEN_SIZE, 8, GLX_BLUE_SIZE, 8,
        GLX_ALPHA_SIZE, alpha_bits,
        GLX_DOUBLEBUFFER, True,
        None
    };
    int count = 0;
    GLXFBConfig *configs = glXChooseFBConfig(dpy, DefaultScreen(dpy),
                                             attributes, &count);
    if (!configs || count < 1) {
        fprintf(stderr, "INFO %s no fbconfig with alpha=%d\n", name,
                alpha_bits);
        return 0;
    }
    /* The first config whose visual depth matches what an opaque or
     * translucent window would be created with. GLX matching is "at least",
     * so an alpha=0 request can still return alpha=8 configs; take the first
     * that actually has the alpha asked for, which is what a toolkit wanting
     * an opaque window does. */
    GLXFBConfig chosen = NULL;
    XVisualInfo *visual = NULL;
    for (int i = 0; i < count && !chosen; ++i) {
        int got_alpha = -1;
        glXGetFBConfigAttrib(dpy, configs[i], GLX_ALPHA_SIZE, &got_alpha);
        if (got_alpha != alpha_bits)
            continue;
        XVisualInfo *candidate = glXGetVisualFromFBConfig(dpy, configs[i]);
        if (!candidate)
            continue;
        if (want_depth && candidate->depth != want_depth) {
            XFree(candidate);
            continue;
        }
        chosen = configs[i];
        visual = candidate;
    }
    if (!chosen) {
        fprintf(stderr, "INFO %s no config with exactly alpha=%d, visual "
                        "depth %d (of %d offered)\n", name, alpha_bits,
                want_depth, count);
        XFree(configs);
        return 0;
    }

    XSetWindowAttributes swa;
    memset(&swa, 0, sizeof(swa));
    swa.colormap = XCreateColormap(dpy, RootWindow(dpy, visual->screen),
                                   visual->visual, AllocNone);
    swa.event_mask = StructureNotifyMask | ExposureMask;
    swa.background_pixel = 0;
    swa.border_pixel = 0;
    Window window = XCreateWindow(dpy, RootWindow(dpy, visual->screen),
                                  0, 0, SIDE, SIDE, 0, visual->depth,
                                  InputOutput, visual->visual,
                                  CWColormap | CWEventMask | CWBackPixel |
                                  CWBorderPixel, &swa);
    XMapWindow(dpy, window);
    /* Wait for the map, or the read below has nothing to read. */
    for (;;) {
        XEvent event;
        XNextEvent(dpy, &event);
        if (event.type == MapNotify && event.xmap.window == window)
            break;
    }
    XSync(dpy, False);

    GLXContext context = glXCreateNewContext(dpy, chosen, GLX_RGBA_TYPE, NULL,
                                             True);
    int ok = 0;
    unsigned long pixel = 0;
    if (context && glXMakeCurrent(dpy, window, context)) {
        glViewport(0, 0, SIDE, SIDE);
        glClearColor(WANT_R / 255.0f, WANT_G / 255.0f, WANT_B / 255.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glXSwapBuffers(dpy, window);
        glFinish();
        XSync(dpy, False);

        /* Through X, not through GL: glReadPixels would answer from the
         * driver's own buffer and pass even when nothing was presented. */
        XImage *image = XGetImage(dpy, window, SIDE / 2, SIDE / 2, 1, 1,
                                  AllPlanes, ZPixmap);
        if (image) {
            pixel = XGetPixel(image, 0, 0);
            const unsigned r = (pixel >> 16) & 0xff;
            const unsigned g = (pixel >> 8) & 0xff;
            const unsigned b = pixel & 0xff;
            ok = r >= WANT_R - 2 && r <= WANT_R + 2 &&
                 g >= WANT_G - 2 && g <= WANT_G + 2 &&
                 b >= WANT_B - 2 && b <= WANT_B + 2;
            fprintf(stderr,
                    "INFO %s alpha=%d visual-depth=%d got=%u,%u,%u "
                    "want=%u,%u,%u\n", name, alpha_bits, visual->depth,
                    r, g, b, WANT_R, WANT_G, WANT_B);
            XDestroyImage(image);
        } else {
            fprintf(stderr, "INFO %s XGetImage failed\n", name);
        }
        glXMakeCurrent(dpy, None, NULL);
    } else {
        fprintf(stderr, "INFO %s no context\n", name);
    }
    if (context)
        glXDestroyContext(dpy, context);
    XDestroyWindow(dpy, window);
    XFree(visual);
    XFree(configs);
    return ok;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) {
        /* No display is not a failure of the driver; say so and skip, the way
         * a headless reference run needs. */
        fprintf(stderr, "INFO no X display; skipping\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }
    fprintf(stderr, "INFO version=%s\n", (const char *)glXGetClientString(
                dpy, GLX_VERSION));
    result("opaque-window", run_case(dpy, "opaque-window", 0, 0));
    result("alpha-window", run_case(dpy, "alpha-window", 8, 24));
    /* The ARGB visual, which is the one this driver got wrong: a config whose
     * visual is depth 32 makes drisw build a depth-32 XImage, and putting
     * that into a window of any other depth is a BadMatch that X reports
     * asynchronously -- success returned, nothing drawn. Here the window is
     * created from that same visual, so the depths agree and it must work. */
    result("translucent-window", run_case(dpy, "translucent-window", 8, 32));
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    XCloseDisplay(dpy);
    return cases == passed ? 0 : 1;
}
