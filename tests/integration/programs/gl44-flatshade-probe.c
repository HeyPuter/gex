/* glShadeModel(GL_FLAT), which was silently ignored.
 *
 * The driver accepted the rasterizer state's `flatshade` bit and did nothing
 * with it, so every flat-shaded triangle came out smooth. `dlist-tri-flat-tri`
 * showed it as a gradient where llvmpipe draws flat red -- found by the
 * llvmpipe/gex_wgpu pixel comparison, since nothing refused and nothing
 * crashed. The fix is to stop claiming the capability, which has st/mesa lower
 * it with nir_lower_flatshade instead.
 *
 * Flat shading takes its value from the provoking vertex, which GL's default
 * convention says is the *last* of the primitive, so a triangle with three
 * different vertex colours is what tells flat from smooth and also tells the
 * right provoking vertex from the wrong one.
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

/* The whole lower-left half of the target is covered by the triangle below,
 * so these three are all inside it and far apart. */
static const struct { unsigned x, y; } inside[3] = {{1, 1}, {5, 1}, {1, 5}};

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

    /* Red, green, blue at the three corners, blue last -- so flat shading
     * paints the whole triangle blue under GL's default convention. */
    for (int flat = 1; flat >= 0; --flat) {
        glShadeModel(flat ? GL_FLAT : GL_SMOOTH);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glBegin(GL_TRIANGLES);
        glColor3f(1.0f, 0.0f, 0.0f); glVertex2f(-1.0f, -1.0f);
        glColor3f(0.0f, 1.0f, 0.0f); glVertex2f(3.0f, -1.0f);
        glColor3f(0.0f, 0.0f, 1.0f); glVertex2f(-1.0f, 3.0f);
        glEnd();
        read_target();
        const GLenum error = glGetError();

        int ok = error == GL_NO_ERROR;
        if (flat) {
            /* Every sample the same, and blue -- the last vertex's colour. */
            for (unsigned i = 0; i < 3; ++i) {
                if (channel(inside[i].x, inside[i].y, 2) < 200) ok = 0;
                if (channel(inside[i].x, inside[i].y, 0) > 40) ok = 0;
                if (channel(inside[i].x, inside[i].y, 1) > 40) ok = 0;
            }
        } else {
            /* Smooth: the three samples differ, and each channel grows
             * towards its own corner. Absolute thresholds would be asserting
             * where the 8x8 window falls inside a triangle that reaches well
             * past it; the *gradient* is the property being tested, and a
             * flat triangle has none. */
            if (channel(5, 1, 1) <= channel(1, 1, 1)) ok = 0;
            if (channel(1, 5, 2) <= channel(1, 1, 2)) ok = 0;
            if (channel(1, 1, 0) <= channel(5, 1, 0)) ok = 0;
        }
        fprintf(stderr,
                "INFO %s (1,1)=%u,%u,%u (5,1)=%u,%u,%u (1,5)=%u,%u,%u "
                "err=0x%04x\n", flat ? "flat" : "smooth",
                channel(1, 1, 0), channel(1, 1, 1), channel(1, 1, 2),
                channel(5, 1, 0), channel(5, 1, 1), channel(5, 1, 2),
                channel(1, 5, 0), channel(1, 5, 1), channel(1, 5, 2),
                (unsigned)error);
        result(flat ? "flat_is_flat" : "smooth_is_smooth", ok);
    }

    /* And through a display list, which is how `dlist-tri-flat-tri` does it:
     * the shade model is compiled into the list along with the triangle, so a
     * driver that only honours the current state gets this one wrong. */
    {
        const GLuint list = glGenLists(1);
        glShadeModel(GL_SMOOTH);
        glNewList(list, GL_COMPILE);
        glShadeModel(GL_FLAT);
        glBegin(GL_TRIANGLES);
        glColor3f(1.0f, 0.0f, 0.0f); glVertex2f(-1.0f, -1.0f);
        glColor3f(0.0f, 1.0f, 0.0f); glVertex2f(3.0f, -1.0f);
        glColor3f(0.0f, 0.0f, 1.0f); glVertex2f(-1.0f, 3.0f);
        glEnd();
        glEndList();
        glShadeModel(GL_SMOOTH);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glCallList(list);
        read_target();
        const GLenum error = glGetError();
        int ok = error == GL_NO_ERROR;
        for (unsigned i = 0; i < 3; ++i) {
            if (channel(inside[i].x, inside[i].y, 2) < 200) ok = 0;
            if (channel(inside[i].x, inside[i].y, 0) > 40) ok = 0;
        }
        fprintf(stderr, "INFO dlist_flat (1,1)=%u,%u,%u (5,1)=%u,%u,%u "
                "err=0x%04x\n",
                channel(1, 1, 0), channel(1, 1, 1), channel(1, 1, 2),
                channel(5, 1, 0), channel(5, 1, 1), channel(5, 1, 2),
                (unsigned)error);
        result("flat_in_display_list", ok);
        glShadeModel(GL_SMOOTH);
    }

    /* A flat varying emitted by a *geometry* shader.
     *
     * This driver serves GL's provoking-vertex convention by reordering the
     * draw's indices, and that rewrite is deliberately skipped when a
     * geometry or tessellation stage is bound: those stages generate the
     * primitives that get rasterized, so the order the *input* vertices
     * arrived in has nothing to do with it -- and rotating a patch draw's
     * vertices broke tessellation outright, since the rewrite also makes the
     * draw indexed and the tessellation path declines an indexed patch draw.
     *
     * The geometry path handles the convention itself, which is what makes
     * that skip safe. This case is here to keep it that way: the triangle is
     * emitted with three different flat colours and must come out the
     * *third* one, GL's last vertex.
     */
    {
        static const char *vs_src =
            "#version 330 core\n"
            "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
        static const char *gs_src =
            "#version 330 core\n"
            "layout(points) in;\n"
            "layout(triangle_strip, max_vertices = 3) out;\n"
            "flat out vec3 tint;\n"
            "void main() {\n"
            "  tint = vec3(1.0, 0.0, 0.0);\n"
            "  gl_Position = vec4(-1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
            "  tint = vec3(0.0, 1.0, 0.0);\n"
            "  gl_Position = vec4( 3.0, -1.0, 0.0, 1.0); EmitVertex();\n"
            "  tint = vec3(0.0, 0.0, 1.0);\n"
            "  gl_Position = vec4(-1.0,  3.0, 0.0, 1.0); EmitVertex();\n"
            "  EndPrimitive();\n"
            "}\n";
        static const char *fs_src =
            "#version 330 core\n"
            "flat in vec3 tint;\n"
            "out vec4 colour;\n"
            "void main() { colour = vec4(tint, 1.0); }\n";
        struct { GLenum stage; const char *src; } parts[] = {
            {GL_VERTEX_SHADER, vs_src}, {GL_GEOMETRY_SHADER, gs_src},
            {GL_FRAGMENT_SHADER, fs_src},
        };
        GLuint prog = glCreateProgram();
        int built = 1;
        for (unsigned i = 0; i < 3 && built; ++i) {
            GLuint sh = glCreateShader(parts[i].stage);
            glShaderSource(sh, 1, &parts[i].src, NULL);
            glCompileShader(sh);
            GLint ok = 0;
            glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
            if (!ok) {
                char log[600] = {0};
                glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
                fprintf(stderr, "INFO gs_flat compile %u: %s\n", i, log);
                built = 0;
            } else {
                glAttachShader(prog, sh);
            }
        }
        GLint linked = 0;
        if (built) {
            glLinkProgram(prog);
            glGetProgramiv(prog, GL_LINK_STATUS, &linked);
            if (!linked) {
                char log[600] = {0};
                glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
                fprintf(stderr, "INFO gs_flat link: %s\n", log);
            }
        }
        if (!built || !linked) {
            result("gs_flat_provoking", 0);
        } else {
            glUseProgram(prog);
            glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_POINTS, 0, 1);
            read_target();
            const unsigned char r = channel(4, 4, 0), g = channel(4, 4, 1),
                                b = channel(4, 4, 2);
            const GLenum error = glGetError();
            fprintf(stderr, "INFO gs_flat_provoking got=%u,%u,%u want=0,0,255 "
                    "err=0x%04x\n", r, g, b, (unsigned)error);
            result("gs_flat_provoking",
                   error == GL_NO_ERROR && b > 160 && r < 60 && g < 60);
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
