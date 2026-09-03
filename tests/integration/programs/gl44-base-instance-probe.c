/* `glDrawArraysInstancedBaseInstance` -- `ARB_base_instance`, GL 4.2, and the
 * only extension that rung still wants.
 *
 * The feature is two claims at once, and they pull in opposite directions:
 *
 *   per-instance attributes are fetched starting at the base instance
 *   gl_InstanceID still counts from zero
 *
 * WebGPU's draw takes a `firstInstance` and this driver already passes it, but
 * SPIR-V's `InstanceIndex` -- which is what `gl_InstanceID` becomes --
 * *counts from it*. So passing the base through gets the attributes right and
 * `gl_InstanceID` wrong, which is exactly why the cap was left off. Advancing
 * each instance-stepped vertex buffer by `base * arrayStride` and passing
 * `firstInstance = 0` satisfies both with no shader change.
 *
 * The probe therefore reads the two independently: the quad's *colour* comes
 * from a per-instance attribute (so it tests the base offset) and its
 * *position* comes from gl_InstanceID (so it tests the zero-basing). A driver
 * that passes the base through as firstInstance draws the right colours in the
 * wrong places; one that ignores the base draws the wrong colours in the right
 * places.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 64

static int has_extension(const char *want) {
    GLint n = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &n);
    if (glGetError() != GL_NO_ERROR) return 0;
    for (GLint i = 0; i < n; ++i) {
        const char *e = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (e && !strcmp(e, want)) return 1;
    }
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int present = has_extension("GL_ARB_base_instance");
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO extension present=%d version=%s\n", present,
            glGetString(GL_VERSION));
    result("extension_present", present);
    if (!present) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    static const char *vs_src =
        "#version 330 core\n"
        "layout(location = 0) in vec2 corner;\n"
        "layout(location = 1) in float tint;\n"
        "flat out float carried;\n"
        "void main() {\n"
        /* Position from gl_InstanceID: instance 0 goes left, 1 right. A
         * driver whose InstanceIndex still carries the base puts them
         * somewhere else entirely. */
        "  float shift = float(gl_InstanceID) * 1.0 - 0.5;\n"
        "  carried = tint;\n"
        "  gl_Position = vec4(corner * 0.25 + vec2(shift, 0.0), 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "flat in float carried;\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(carried, 0.0, 0.0, 1.0); }\n";

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint linked = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 2;
    }

    GLuint colour_tex = 0, fbo = 0;
    glGenTextures(1, &colour_tex);
    glBindTexture(GL_TEXTURE_2D, colour_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour_tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);

    GLuint vao = 0, corners = 0, tints = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    static const float quad[8] = {-1, -1, 1, -1, -1, 1, 1, 1};
    glGenBuffers(1, &corners);
    glBindBuffer(GL_ARRAY_BUFFER, corners);
    glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    /* Six per-instance tints. The draw asks for two instances at base 3, so
     * the pair used must be entries 3 and 4 -- 0.6 and 0.8 -- and never
     * entries 0 and 1. */
    static const float tint_data[6] = {0.1f, 0.2f, 0.4f, 0.6f, 0.8f, 1.0f};
    glGenBuffers(1, &tints);
    glBindBuffer(GL_ARRAY_BUFFER, tints);
    glBufferData(GL_ARRAY_BUFFER, sizeof(tint_data), tint_data,
                 GL_STATIC_DRAW);
    glEnableVertexAttribArray(1);
    glVertexAttribPointer(1, 1, GL_FLOAT, GL_FALSE, 0, NULL);
    glVertexAttribDivisor(1, 1);

    glUseProgram(prog);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArraysInstancedBaseInstance(GL_TRIANGLE_STRIP, 0, 4, 2, 3);

    unsigned char left[4] = {0}, right[4] = {0};
    /* Instance 0 sits at x = -0.5, instance 1 at x = +0.5. */
    glReadPixels(SIDE / 4, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, left);
    glReadPixels(3 * SIDE / 4, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                 right);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO base_instance left=%u want=153 right=%u want=204 "
            "err=0x%04x\n", left[0], right[0], (unsigned)error);
    /* 0.6 -> 153 and 0.8 -> 204. Entries 0 and 1 would be 26 and 51, and a
     * base carried into gl_InstanceID would leave both samples black. */
    result("attribute_takes_base",
           error == GL_NO_ERROR && left[0] >= 150 && left[0] <= 156 &&
           right[0] >= 201 && right[0] <= 207);
    result("instance_id_from_zero",
           error == GL_NO_ERROR && left[0] > 8 && right[0] > 8);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
