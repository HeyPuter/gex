/* The fixed-function calls the guest could not make at all.
 *
 * libGL exports a fixed list of symbols, so a program calling one that is
 * absent fails to resolve it and never starts -- and 181 of the 341 installed
 * GL programs needed at least one the guest did not export. None of that is
 * visible in a native sweep, where they link the system's own libGL, so this
 * probe exists to be run through the guest: the native run is the reference
 * and the browser run is the transport under test.
 *
 * Each case is checkable from pixels rather than from a state query, because a
 * transport that drops a command silently leaves the state query answering
 * whatever the host defaults to.
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

/* A 4x4 target, read back whole. Four pixels is enough for a coverage test
 * and small enough to print when one fails. */
static unsigned char pixels[4 * 4 * 4];

static unsigned char red(unsigned x, unsigned y) {
    return pixels[(y * 4 + x) * 4];
}
static unsigned char green(unsigned x, unsigned y) {
    return pixels[(y * 4 + x) * 4 + 1];
}

static void read_target(void) {
    memset(pixels, 0xAB, sizeof(pixels));
    glReadPixels(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
}

static void clear_black(void) {
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
}

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
    /* Empty: a compatibility profile, which is where all of this lives. */
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
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 4, 4, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 4, 4);

    /* An ortho projection of exactly the 4x4 grid, so a rectangle in object
     * coordinates lands on named pixels. */
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    glOrtho(0.0, 4.0, 0.0, 4.0, -1.0, 1.0);
    glMatrixMode(GL_MODELVIEW);
    glLoadIdentity();

    /* 1. glOrtho and glRectf together: the middle two columns and rows. */
    clear_black();
    glColor3f(1.0f, 1.0f, 1.0f);
    glRectf(1.0f, 1.0f, 3.0f, 3.0f);
    read_target();
    {
        int ok = glGetError() == GL_NO_ERROR;
        for (unsigned y = 0; y < 4; ++y)
            for (unsigned x = 0; x < 4; ++x) {
                const int inside = x >= 1 && x < 3 && y >= 1 && y < 3;
                if ((red(x, y) > 128) != inside) ok = 0;
            }
        fprintf(stderr, "INFO ortho_rect row1=%u,%u,%u,%u\n",
                red(0, 1), red(1, 1), red(2, 1), red(3, 1));
        result("ortho_and_rect", ok);
    }

    /* 2. glColor3f, through immediate mode rather than glRectf. */
    clear_black();
    glColor3f(0.0f, 1.0f, 0.0f);
    glBegin(GL_TRIANGLES);
    glVertex2f(0.0f, 0.0f);
    glVertex2f(4.0f, 0.0f);
    glVertex2f(0.0f, 4.0f);
    glEnd();
    read_target();
    {
        const int ok = glGetError() == GL_NO_ERROR && green(0, 0) > 200 &&
                       red(0, 0) < 40;
        fprintf(stderr, "INFO colour3f rgba=%u,%u err=0x%04x\n", red(0, 0),
                green(0, 0), (unsigned)glGetError());
        result("color3f_immediate", ok);
    }

    /* 3. glScalef: the same unit rectangle scaled to cover the grid. */
    clear_black();
    glColor3f(1.0f, 1.0f, 1.0f);
    glPushMatrix();
    glScalef(4.0f, 4.0f, 1.0f);
    glRectf(0.0f, 0.0f, 1.0f, 1.0f);
    glPopMatrix();
    read_target();
    {
        int ok = glGetError() == GL_NO_ERROR;
        for (unsigned y = 0; y < 4; ++y)
            for (unsigned x = 0; x < 4; ++x)
                if (red(x, y) <= 128) ok = 0;
        result("scalef", ok);
    }

    /* 4. glLoadMatrixf: the same again through an explicit matrix, which is
     * the other way a GL2-era program sets a transform. Column-major, so the
     * translation is the last row. */
    clear_black();
    {
        static const GLfloat scale4[16] = {
            4.0f, 0.0f, 0.0f, 0.0f,
            0.0f, 4.0f, 0.0f, 0.0f,
            0.0f, 0.0f, 1.0f, 0.0f,
            0.0f, 0.0f, 0.0f, 1.0f,
        };
        glLoadMatrixf(scale4);
        glColor3f(1.0f, 1.0f, 1.0f);
        glRectf(0.0f, 0.0f, 1.0f, 1.0f);
        glLoadIdentity();
        read_target();
        int ok = glGetError() == GL_NO_ERROR;
        for (unsigned y = 0; y < 4; ++y)
            for (unsigned x = 0; x < 4; ++x)
                if (red(x, y) <= 128) ok = 0;
        result("loadmatrixf", ok);
    }

    /* 5. glAlphaFunc: an alpha of 0.25 against a reference of 0.5 discards
     * everything, and 0.75 keeps it. */
    {
        glEnable(GL_ALPHA_TEST);
        glAlphaFunc(GL_GREATER, 0.5f);
        clear_black();
        glColor4f(1.0f, 1.0f, 1.0f, 0.25f);
        glRectf(0.0f, 0.0f, 4.0f, 4.0f);
        read_target();
        const int discarded = red(1, 1) < 40;
        clear_black();
        glColor4f(1.0f, 1.0f, 1.0f, 0.75f);
        glRectf(0.0f, 0.0f, 4.0f, 4.0f);
        read_target();
        const int kept = red(1, 1) > 200;
        glDisable(GL_ALPHA_TEST);
        fprintf(stderr, "INFO alpha_test discarded=%d kept=%d err=0x%04x\n",
                discarded, kept, (unsigned)glGetError());
        result("alpha_func", discarded && kept);
    }

    /* 6. glPushAttrib and glPopAttrib around a colour change. */
    clear_black();
    glColor3f(0.0f, 1.0f, 0.0f);
    glPushAttrib(GL_CURRENT_BIT);
    glColor3f(1.0f, 0.0f, 0.0f);
    glPopAttrib();
    glRectf(0.0f, 0.0f, 4.0f, 4.0f);
    read_target();
    {
        const int ok = glGetError() == GL_NO_ERROR && green(1, 1) > 200 &&
                       red(1, 1) < 40;
        fprintf(stderr, "INFO push_attrib rg=%u,%u\n", red(1, 1), green(1, 1));
        result("push_pop_attrib", ok);
    }

    /* 7. glClipPlane: x >= 2 survives, so the left half is clipped away. */
    {
        static const GLdouble plane[4] = {1.0, 0.0, 0.0, -2.0};
        glClipPlane(GL_CLIP_PLANE0, plane);
        glEnable(GL_CLIP_PLANE0);
        clear_black();
        glColor3f(1.0f, 1.0f, 1.0f);
        glRectf(0.0f, 0.0f, 4.0f, 4.0f);
        read_target();
        glDisable(GL_CLIP_PLANE0);
        int ok = glGetError() == GL_NO_ERROR;
        for (unsigned y = 0; y < 4; ++y)
            for (unsigned x = 0; x < 4; ++x) {
                const int inside = x >= 2;
                if ((red(x, y) > 128) != inside) ok = 0;
            }
        fprintf(stderr, "INFO clip_plane row1=%u,%u,%u,%u\n",
                red(0, 1), red(1, 1), red(2, 1), red(3, 1));
        result("clip_plane", ok);
    }

    /* 8. glFogfv and glFogf: linear fog from 0 to 1 in eye space, with the
     * geometry at z = -0.5, blends the fragment halfway to the fog colour.
     * A dropped fog command leaves the fragment its own colour. */
    {
        static const GLfloat fog_colour[4] = {0.0f, 1.0f, 0.0f, 1.0f};
        glFogi(GL_FOG_MODE, GL_LINEAR);
        glFogfv(GL_FOG_COLOR, fog_colour);
        glFogf(GL_FOG_START, 0.0f);
        glFogf(GL_FOG_END, 1.0f);
        glEnable(GL_FOG);
        clear_black();
        glColor3f(1.0f, 0.0f, 0.0f);
        glMatrixMode(GL_MODELVIEW);
        glLoadIdentity();
        glTranslatef(0.0f, 0.0f, -0.5f);
        glRectf(0.0f, 0.0f, 4.0f, 4.0f);
        glLoadIdentity();
        read_target();
        glDisable(GL_FOG);
        const unsigned char r = red(1, 1), g = green(1, 1);
        /* Half way: both channels near 128. Generous, because the fog
         * factor depends on how the eye distance is computed. */
        const int ok = glGetError() == GL_NO_ERROR && r > 80 && r < 180 &&
                       g > 80 && g < 180;
        fprintf(stderr, "INFO fog rg=%u,%u\n", r, g);
        result("fog", ok);
    }

    /* 9. glTexGen: texture coordinates generated from the object position
     * rather than supplied. GL_OBJECT_LINEAR with the planes below makes
     * s = x/4 and t = y/4, so the 0..4 ortho box maps exactly onto the
     * texture -- and each texel lands on one quadrant of the 4x4 target,
     * which a wrong plane or a dropped enable cannot imitate. */
    {
        static const unsigned char texels[16] = {
            255, 0, 0, 255,      0, 255, 0, 255,
            0, 0, 255, 255,      255, 255, 0, 255,
        };
        static const GLfloat s_plane[4] = {0.25f, 0.0f, 0.0f, 0.0f};
        static const GLfloat t_plane[4] = {0.0f, 0.25f, 0.0f, 0.0f};
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
        glTexGeni(GL_S, GL_TEXTURE_GEN_MODE, GL_OBJECT_LINEAR);
        glTexGeni(GL_T, GL_TEXTURE_GEN_MODE, GL_OBJECT_LINEAR);
        glTexGenfv(GL_S, GL_OBJECT_PLANE, s_plane);
        glTexGenfv(GL_T, GL_OBJECT_PLANE, t_plane);
        glEnable(GL_TEXTURE_GEN_S);
        glEnable(GL_TEXTURE_GEN_T);
        clear_black();
        glColor3f(1.0f, 1.0f, 1.0f);
        glRectf(0.0f, 0.0f, 4.0f, 4.0f);
        read_target();
        glDisable(GL_TEXTURE_GEN_S);
        glDisable(GL_TEXTURE_GEN_T);
        glDisable(GL_TEXTURE_2D);
        const int ok = glGetError() == GL_NO_ERROR &&
                       red(0, 0) > 200 && green(0, 0) < 60 &&
                       green(3, 0) > 200 && red(3, 0) < 60 &&
                       red(3, 3) > 200 && green(3, 3) > 200;
        fprintf(stderr, "INFO texgen bl=%u,%u br=%u,%u tl=%u,%u tr=%u,%u\n",
                red(0, 0), green(0, 0), red(3, 0), green(3, 0),
                red(0, 3), green(0, 3), red(3, 3), green(3, 3));
        result("texgen_object_linear", ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
