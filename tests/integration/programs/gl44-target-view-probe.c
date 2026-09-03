/* Rendering *through* a view whose format is not the texture's.
 *
 * ARB_texture_view's other half. The texture-view probe next door reads a
 * reinterpreting view through a sampler; this one attaches it to a
 * framebuffer and writes through it, which is a different path in a driver
 * and the one Blender actually depends on: it renders object ids as R16UI
 * into an R16F texture, and reinterprets two more of its colour targets the
 * same way.
 *
 * WebGPU has no view across a format class, so there is nothing to attach.
 * The driver renders into a scratch texture of the *surface's* format instead
 * and copies the bytes back -- a copy through a buffer moves bytes without
 * regard to format, and the two formats here are both 16 bits per texel. What
 * has to be true for that to be correct is exactly what this probe checks:
 * that bytes written through the view are afterwards visible through the
 * *base* texture, because that is what a later pass samples.
 *
 * So each case writes through an R16UI view and reads back through the R16F
 * texture with a sampler. 0x3C00 is half-float 1.0, which arrives as 255
 * after the read shader scales it -- a value no plausible failure produces by
 * accident. A driver that writes into the wrong texture, or into a scratch it
 * never copies back, reads 0.
 *
 *   `render`  a draw through the view
 *   `clear`   glClearBufferuiv through the view, which is a separate path
 *   `reclear` clear, then draw, then clear again: the copy back has to happen
 *             for each in order rather than once at the end
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

#define SIDE 8

static int have_extension(const char *name) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    for (GLint i = 0; i < count; ++i) {
        const GLubyte *one = glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (one && strcmp((const char *)one, name) == 0) return 1;
    }
    return 0;
}

static GLuint build(const char *fs_body, const char *out_decl) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    char fs_src[512];
    snprintf(fs_src, sizeof(fs_src),
             "#version 330 core\n"
             "uniform sampler2D src;\n"
             "%s\n"
             "void main() { %s }\n", out_decl, fs_body);
    const char *fs_ptr = fs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_ptr, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
        glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[768] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return prog;
}

/* Samples the R16F base texture and scales it to a byte, so the answer comes
 * back through an ordinary RGBA8 read rather than a float readback. */
static unsigned read_base(GLuint read_prog, GLuint base_tex, GLuint out_fbo) {
    glBindFramebuffer(GL_FRAMEBUFFER, out_fbo);
    glViewport(0, 0, SIDE, SIDE);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(read_prog);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, base_tex);
    glUniform1i(glGetUniformLocation(read_prog, "src"), 0);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    return px[0];
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

    fprintf(stderr, "INFO version=%s texture_view=%d\n",
            glGetString(GL_VERSION), have_extension("GL_ARB_texture_view"));

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* The readback target: an ordinary RGBA8 texture. */
    GLuint out_tex = 0, out_fbo = 0;
    glGenTextures(1, &out_tex);
    glBindTexture(GL_TEXTURE_2D, out_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &out_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, out_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, out_tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;

    /* glTextureView needs an immutable-format texture. */
    GLuint base_tex = 0;
    glGenTextures(1, &base_tex);
    glBindTexture(GL_TEXTURE_2D, base_tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R16F, SIDE, SIDE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    GLuint view_tex = 0;
    glGenTextures(1, &view_tex);
    /* R16F and R16UI are both VIEW_CLASS_16_BITS, so this is a legal view of
     * the same bits under another type -- and the one WebGPU cannot make. */
    glTextureView(view_tex, GL_TEXTURE_2D, base_tex, GL_R16UI, 0, 1, 0, 1);
    if (glGetError() != GL_NO_ERROR) {
        fprintf(stderr, "INFO glTextureView refused R16UI over R16F\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }
    glBindTexture(GL_TEXTURE_2D, view_tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    GLuint view_fbo = 0;
    glGenFramebuffers(1, &view_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, view_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, view_tex, 0);
    const GLenum view_status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    fprintf(stderr, "INFO view fbo status=0x%04x\n", (unsigned)view_status);
    if (view_status != GL_FRAMEBUFFER_COMPLETE) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    /* 0x3C00 is half-float 1.0. The read shader multiplies by 255, so a
     * correct round trip is 255 and a texture that was never written is 0. */
    GLuint write_prog = build("colour = uint(15360);", "out uint colour;");
    GLuint read_prog =
        build("colour = vec4(texture(src, vec2(0.5)).r, 0.0, 0.0, 1.0);",
              "out vec4 colour;");
    if (!write_prog || !read_prog) {
        fprintf(stderr, "INFO shaders failed to build\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    /* render: a draw through the view. */
    glBindFramebuffer(GL_FRAMEBUFFER, view_fbo);
    glViewport(0, 0, SIDE, SIDE);
    glUseProgram(write_prog);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned got = read_base(read_prog, base_tex, out_fbo);
    GLenum error = glGetError();
    fprintf(stderr, "INFO render got=%u want=255 err=0x%04x\n", got,
            (unsigned)error);
    result("render", error == GL_NO_ERROR && got >= 253);

    /* clear: the same bit pattern through glClearBufferuiv, which does not go
     * through a draw at all. */
    glBindFramebuffer(GL_FRAMEBUFFER, view_fbo);
    glViewport(0, 0, SIDE, SIDE);
    {
        const GLuint zero[4] = {0, 0, 0, 0};
        glClearBufferuiv(GL_COLOR, 0, zero);
    }
    got = read_base(read_prog, base_tex, out_fbo);
    const unsigned cleared = got;
    glBindFramebuffer(GL_FRAMEBUFFER, view_fbo);
    {
        const GLuint one[4] = {15360u, 0, 0, 0};
        glClearBufferuiv(GL_COLOR, 0, one);
    }
    got = read_base(read_prog, base_tex, out_fbo);
    error = glGetError();
    fprintf(stderr, "INFO clear zeroed=%u got=%u want=0/255 err=0x%04x\n",
            cleared, got, (unsigned)error);
    result("clear", error == GL_NO_ERROR && cleared <= 2 && got >= 253);

    /* reclear: clear to zero, draw, clear again. Each has to reach the base
     * texture in order -- a driver that copies its scratch back only once
     * ends up with whichever happened last, whatever the order was. */
    glBindFramebuffer(GL_FRAMEBUFFER, view_fbo);
    glViewport(0, 0, SIDE, SIDE);
    {
        const GLuint zero[4] = {0, 0, 0, 0};
        glClearBufferuiv(GL_COLOR, 0, zero);
    }
    glUseProgram(write_prog);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    const unsigned after_draw = read_base(read_prog, base_tex, out_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, view_fbo);
    {
        const GLuint zero[4] = {0, 0, 0, 0};
        glClearBufferuiv(GL_COLOR, 0, zero);
    }
    const unsigned after_clear = read_base(read_prog, base_tex, out_fbo);
    error = glGetError();
    fprintf(stderr, "INFO reclear after_draw=%u after_clear=%u want=255/0 "
            "err=0x%04x\n", after_draw, after_clear, (unsigned)error);
    result("reclear", error == GL_NO_ERROR && after_draw >= 253 &&
                      after_clear <= 2);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
