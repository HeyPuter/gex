/* The fixed-function vertex arrays, from client memory.
 *
 * glVertexPointer and its siblings are how every pre-VBO GL program submits
 * geometry, and the guest exported none of them -- which is not a missing
 * feature but a missing symbol: the program fails to resolve it and never
 * starts. So this probe exists to be run through the guest, where the arrays
 * have to be copied into buffers the host can read; the native run is the
 * reference.
 *
 * Every case is read back as pixels. A transport that drops an array leaves
 * glGet answering whatever the host defaults to, and a transport that copies
 * the wrong bytes draws something -- just not the right thing.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static unsigned char pixels[8 * 8 * 4];

static void read_target(void) {
    memset(pixels, 0xAB, sizeof(pixels));
    glReadPixels(0, 0, 8, 8, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
}
static unsigned char channel(unsigned x, unsigned y, unsigned c) {
    return pixels[(y * 8 + x) * 4 + c];
}
static void clear_black(void) {
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
}

/* A quad covering the whole target, as two triangles. The colours are chosen
 * so that each corner is dominated by one channel and the centre by none:
 * a copy that loses the stride, or reads from the wrong element, lands on a
 * different colour rather than a plausible one. */
static const float quad_xy[12] = {
    -1.0f, -1.0f,   1.0f, -1.0f,   -1.0f, 1.0f,
     1.0f, -1.0f,   1.0f,  1.0f,   -1.0f, 1.0f,
};
static const float quad_rgb[18] = {
    1.0f, 0.0f, 0.0f,   0.0f, 1.0f, 0.0f,   0.0f, 0.0f, 1.0f,
    0.0f, 1.0f, 0.0f,   1.0f, 1.0f, 1.0f,   0.0f, 0.0f, 1.0f,
};

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    /* Empty: a compatibility profile, which is where these arrays live. */
    const EGLint context_attributes[] = {EGL_NONE};
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 8, 8, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 8, 8);
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    glMatrixMode(GL_MODELVIEW);
    glLoadIdentity();

    /* 1. Two separate tightly-packed client arrays. */
    clear_black();
    glEnableClientState(GL_VERTEX_ARRAY);
    glEnableClientState(GL_COLOR_ARRAY);
    glVertexPointer(2, GL_FLOAT, 0, quad_xy);
    glColorPointer(3, GL_FLOAT, 0, quad_rgb);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    read_target();
    {
        /* Bottom-left is the red vertex, bottom-right green, top-left blue,
         * and the top-right corner white. */
        const int ok = glGetError() == GL_NO_ERROR &&
                       channel(0, 0, 0) > 180 && channel(7, 0, 1) > 180 &&
                       channel(0, 7, 2) > 180 && channel(7, 7, 0) > 180 &&
                       channel(7, 7, 1) > 180 && channel(7, 7, 2) > 180;
        fprintf(stderr,
                "INFO separate bl=%u,%u,%u br=%u,%u,%u tl=%u,%u,%u "
                "tr=%u,%u,%u\n",
                channel(0, 0, 0), channel(0, 0, 1), channel(0, 0, 2),
                channel(7, 0, 0), channel(7, 0, 1), channel(7, 0, 2),
                channel(0, 7, 0), channel(0, 7, 1), channel(0, 7, 2),
                channel(7, 7, 0), channel(7, 7, 1), channel(7, 7, 2));
        result("separate_arrays", ok);
    }

    /* 2. One interleaved array, so the stride is not the element size. A copy
     * that ignores the stride reads positions out of the colour slots. */
    {
        struct { float x, y, r, g, b; } interleaved[6];
        for (unsigned i = 0; i < 6; ++i) {
            interleaved[i].x = quad_xy[i * 2 + 0];
            interleaved[i].y = quad_xy[i * 2 + 1];
            interleaved[i].r = quad_rgb[i * 3 + 0];
            interleaved[i].g = quad_rgb[i * 3 + 1];
            interleaved[i].b = quad_rgb[i * 3 + 2];
        }
        clear_black();
        glVertexPointer(2, GL_FLOAT, sizeof(interleaved[0]), &interleaved[0].x);
        glColorPointer(3, GL_FLOAT, sizeof(interleaved[0]), &interleaved[0].r);
        glDrawArrays(GL_TRIANGLES, 0, 6);
        read_target();
        const int ok = glGetError() == GL_NO_ERROR &&
                       channel(0, 0, 0) > 180 && channel(7, 0, 1) > 180 &&
                       channel(0, 7, 2) > 180;
        fprintf(stderr, "INFO interleaved bl=%u,%u,%u br=%u,%u,%u\n",
                channel(0, 0, 0), channel(0, 0, 1), channel(0, 0, 2),
                channel(7, 0, 0), channel(7, 0, 1), channel(7, 0, 2));
        result("interleaved_stride", ok);
    }

    /* 3. glDrawElements with client indices, over the same client arrays --
     * two ways of pointing at guest memory in one draw. The indices name only
     * the first triangle, so the top-right corner stays black. */
    {
        static const unsigned short indices[3] = {0, 1, 2};
        clear_black();
        glVertexPointer(2, GL_FLOAT, 0, quad_xy);
        glColorPointer(3, GL_FLOAT, 0, quad_rgb);
        glDrawElements(GL_TRIANGLES, 3, GL_UNSIGNED_SHORT, indices);
        read_target();
        const int ok = glGetError() == GL_NO_ERROR &&
                       channel(0, 0, 0) > 180 && channel(7, 7, 0) < 40 &&
                       channel(7, 7, 1) < 40 && channel(7, 7, 2) < 40;
        fprintf(stderr, "INFO elements bl=%u,%u,%u tr=%u,%u,%u\n",
                channel(0, 0, 0), channel(0, 0, 1), channel(0, 0, 2),
                channel(7, 7, 0), channel(7, 7, 1), channel(7, 7, 2));
        result("draw_elements_client", ok);
    }

    /* 4. glTexCoordPointer against a 2x2 texture, with the colour array off
     * so the texture is the only thing that can colour the quad. */
    {
        static const unsigned char texels[16] = {
            255, 0, 0, 255,     0, 255, 0, 255,
            0, 0, 255, 255,     255, 255, 0, 255,
        };
        static const float uv[12] = {
            0.25f, 0.25f,  0.75f, 0.25f,  0.25f, 0.75f,
            0.75f, 0.25f,  0.75f, 0.75f,  0.25f, 0.75f,
        };
        GLuint texture = 0;
        glGenTextures(1, &texture);
        glBindTexture(GL_TEXTURE_2D, texture);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
        glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 2, 2, 0, GL_RGBA,
                     GL_UNSIGNED_BYTE, texels);
        glEnable(GL_TEXTURE_2D);
        glDisableClientState(GL_COLOR_ARRAY);
        glColor3f(1.0f, 1.0f, 1.0f);
        glEnableClientState(GL_TEXTURE_COORD_ARRAY);
        clear_black();
        glVertexPointer(2, GL_FLOAT, 0, quad_xy);
        glTexCoordPointer(2, GL_FLOAT, 0, uv);
        glDrawArrays(GL_TRIANGLES, 0, 6);
        read_target();
        glDisable(GL_TEXTURE_2D);
        glDisableClientState(GL_TEXTURE_COORD_ARRAY);
        /* The four texels land one per quadrant: red bottom-left, green
         * bottom-right, blue top-left, yellow top-right. */
        const int ok = glGetError() == GL_NO_ERROR &&
                       channel(1, 1, 0) > 180 && channel(1, 1, 1) < 60 &&
                       channel(6, 1, 1) > 180 && channel(6, 1, 0) < 60 &&
                       channel(1, 6, 2) > 180 &&
                       channel(6, 6, 0) > 180 && channel(6, 6, 1) > 180;
        fprintf(stderr, "INFO texcoord bl=%u,%u,%u br=%u,%u,%u tl=%u,%u,%u "
                "tr=%u,%u,%u\n",
                channel(1, 1, 0), channel(1, 1, 1), channel(1, 1, 2),
                channel(6, 1, 0), channel(6, 1, 1), channel(6, 1, 2),
                channel(1, 6, 0), channel(1, 6, 1), channel(1, 6, 2),
                channel(6, 6, 0), channel(6, 6, 1), channel(6, 6, 2));
        result("texcoord_array", ok);
    }

    /* 5. Disabling the arrays again, which has to reach the host: a stale
     * enabled colour array over a freed pointer is how this goes wrong in an
     * application rather than in a probe. */
    {
        glDisableClientState(GL_VERTEX_ARRAY);
        clear_black();
        glDrawArrays(GL_TRIANGLES, 0, 6);
        read_target();
        int ok = glGetError() == GL_NO_ERROR;
        for (unsigned y = 0; y < 8; ++y)
            for (unsigned x = 0; x < 8; ++x)
                if (channel(x, y, 0) > 40 || channel(x, y, 1) > 40) ok = 0;
        result("disabled_draws_nothing", ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
