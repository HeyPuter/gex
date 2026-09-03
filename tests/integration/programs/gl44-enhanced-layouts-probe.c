/* ARB_enhanced_layouts -- one of GL 4.4's last two.
 *
 * Four separate things under one name, and the interesting question is how
 * many of them a driver already supports without knowing it:
 *
 *   `component`   `layout(location = 0, component = 0)` and `component = 2`
 *                 on two varyings that share one location. The GLSL linker
 *                 already packs small varyings this way *without* the
 *                 extension -- what the extension adds is the application
 *                 asking for a specific packing -- so a driver that handles
 *                 `location_frac` at all handles this.
 *   `block-offset` `layout(offset = N)` on a uniform block member, which puts
 *                 a scalar at a byte offset the application chose rather than
 *                 one std140 picked.
 *   `xfb`         `layout(xfb_buffer, xfb_offset)` in the shader instead of
 *                 glTransformFeedbackVaryings. The linker turns both into the
 *                 same transform-feedback description, so a driver reading
 *                 that description gets this for nothing.
 *   `const-expr`  a layout qualifier computed from a constant expression,
 *                 which is a front-end feature and reaches no driver at all.
 *
 * An explicit `location` on a *vertex output* needs
 * GL_ARB_separate_shader_objects below GLSL 4.20, which every case here asks
 * for by name -- without it the component cases compiled and then read zero,
 * because the outputs and inputs were never matched.
 *
 * Every case checks a value rather than that the shader compiled: a component
 * qualifier that is *accepted* and then ignored puts the second varying's
 * value where the first's should be, which compiles perfectly.
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

static int have_extension(const char *want)
{
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

#define SIDE 8

static GLuint link_pair(const char *vs_src, const char *fs_src,
                        const char *what)
{
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(vs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s vs: %s\n", what, log);
        glDeleteShader(vs);
        return 0;
    }
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s fs: %s\n", what, log);
        glDeleteShader(vs);
        glDeleteShader(fs);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glDeleteShader(vs);
    glDeleteShader(fs);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s link: %s\n", what, log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

static const char *quad_body =
    "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
    "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n";

static void read_centre(unsigned char px[4])
{
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
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

    const int have = have_extension("GL_ARB_enhanced_layouts");
    fprintf(stderr, "INFO version=%s enhanced_layouts=%d\n",
            glGetString(GL_VERSION), have);

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);

    /* Case 1: two varyings sharing one location by explicit component. If the
     * qualifier is ignored they collide and one value overwrites the other,
     * so the two channels must carry *different* numbers. */
    {
        char vs[900], fs[700];
        snprintf(vs, sizeof(vs),
                 "#version 330 core\n"
                 "#extension GL_ARB_enhanced_layouts : require\n"
                 "#extension GL_ARB_separate_shader_objects : require\n"
                 "layout(location = 0, component = 0) out float low;\n"
                 "layout(location = 0, component = 2) out vec2 high;\n"
                 "void main() {\n%s"
                 "  low = 0.25;\n"
                 "  high = vec2(0.5, 0.75);\n"
                 "}\n", quad_body);
        snprintf(fs, sizeof(fs),
                 "#version 330 core\n"
                 "#extension GL_ARB_enhanced_layouts : require\n"
                 "#extension GL_ARB_separate_shader_objects : require\n"
                 "layout(location = 0, component = 0) in float low;\n"
                 "layout(location = 0, component = 2) in vec2 high;\n"
                 "out vec4 colour;\n"
                 "void main() { colour = vec4(low, high, 1.0); }\n");
        unsigned char px[4] = {0};
        GLuint prog = have ? link_pair(vs, fs, "component") : 0;
        if (prog) {
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            read_centre(px);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO component got=%u,%u,%u want=64,128,191 "
                "err=0x%04x\n", px[0], px[1], px[2], (unsigned)error);
        result("component", have && prog && error == GL_NO_ERROR &&
                            px[0] >= 62 && px[0] <= 66 &&
                            px[1] >= 126 && px[1] <= 130 &&
                            px[2] >= 189 && px[2] <= 193);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 2: an explicit member offset in a uniform block. The second member
     * is put at byte 16 by hand, which is where std140 would have placed it
     * anyway -- so the third is put at 32 with a hole at 20..31 that no
     * automatic layout would leave. */
    {
        char vs[700], fs[900];
        snprintf(vs, sizeof(vs),
                 "#version 330 core\n"
                 "void main() {\n%s}\n", quad_body);
        snprintf(fs, sizeof(fs),
                 "#version 330 core\n"
                 "#extension GL_ARB_enhanced_layouts : require\n"
                 "#extension GL_ARB_separate_shader_objects : require\n"
                 "layout(std140) uniform Block {\n"
                 "  layout(offset = 0) float a;\n"
                 "  layout(offset = 16) float b;\n"
                 "  layout(offset = 32) float c;\n"
                 "};\n"
                 "out vec4 colour;\n"
                 "void main() { colour = vec4(a, b, c, 1.0); }\n");
        unsigned char px[4] = {0};
        GLuint prog = have ? link_pair(vs, fs, "block-offset") : 0;
        GLuint ubo = 0;
        if (prog) {
            float words[12] = {0};
            words[0] = 0.25f;   /* byte 0  */
            words[4] = 0.5f;    /* byte 16 */
            words[8] = 0.75f;   /* byte 32 */
            glGenBuffers(1, &ubo);
            glBindBuffer(GL_UNIFORM_BUFFER, ubo);
            glBufferData(GL_UNIFORM_BUFFER, sizeof(words), words,
                         GL_STATIC_DRAW);
            glBindBufferBase(GL_UNIFORM_BUFFER, 0, ubo);
            const GLuint index = glGetUniformBlockIndex(prog, "Block");
            if (index != GL_INVALID_INDEX)
                glUniformBlockBinding(prog, index, 0);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            read_centre(px);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO block-offset got=%u,%u,%u want=64,128,191 "
                "err=0x%04x\n", px[0], px[1], px[2], (unsigned)error);
        result("block-offset", have && prog && error == GL_NO_ERROR &&
                               px[0] >= 62 && px[0] <= 66 &&
                               px[1] >= 126 && px[1] <= 130 &&
                               px[2] >= 189 && px[2] <= 193);
        if (prog) glDeleteProgram(prog);
        if (ubo) glDeleteBuffers(1, &ubo);
    }

    /* Case 3: transform feedback declared in the shader. No
     * glTransformFeedbackVaryings call at all -- the layout qualifiers are
     * the whole specification of the capture. */
    {
        char vs[900], fs[500];
        snprintf(vs, sizeof(vs),
                 "#version 330 core\n"
                 "#extension GL_ARB_enhanced_layouts : require\n"
                 "#extension GL_ARB_separate_shader_objects : require\n"
                 "layout(xfb_buffer = 0, xfb_offset = 0) out vec4 captured;\n"
                 "void main() {\n%s"
                 "  captured = vec4(1.0, 2.0, 3.0, 4.0);\n"
                 "}\n", quad_body);
        snprintf(fs, sizeof(fs),
                 "#version 330 core\n"
                 "in vec4 captured;\n"
                 "out vec4 colour;\n"
                 "void main() { colour = vec4(0.0, 0.0, 0.0, 1.0); }\n");
        float got[4] = {-1, -1, -1, -1};
        GLuint prog = have ? link_pair(vs, fs, "xfb") : 0;
        GLuint tfbo = 0;
        if (prog) {
            glGenBuffers(1, &tfbo);
            glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, tfbo);
            glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, 16 * 4, NULL,
                         GL_DYNAMIC_COPY);
            glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, tfbo);
            glUseProgram(prog);
            glEnable(GL_RASTERIZER_DISCARD);
            glBeginTransformFeedback(GL_POINTS);
            glDrawArrays(GL_POINTS, 0, 1);
            glEndTransformFeedback();
            glDisable(GL_RASTERIZER_DISCARD);
            const float *read = (const float *)glMapBufferRange(
                GL_TRANSFORM_FEEDBACK_BUFFER, 0, 16, GL_MAP_READ_BIT);
            if (read) {
                memcpy(got, read, sizeof(got));
                glUnmapBuffer(GL_TRANSFORM_FEEDBACK_BUFFER);
            }
            glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, 0);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO xfb got=%g,%g,%g,%g want=1,2,3,4 err=0x%04x\n",
                got[0], got[1], got[2], got[3], (unsigned)error);
        result("xfb", have && prog && error == GL_NO_ERROR &&
                      got[0] == 1.0f && got[1] == 2.0f && got[2] == 3.0f &&
                      got[3] == 4.0f);
        if (prog) glDeleteProgram(prog);
        if (tfbo) glDeleteBuffers(1, &tfbo);
    }

    /* Case 4: a constant expression in a layout qualifier, which is purely a
     * front-end feature -- included because "the extension is advertised" and
     * "the compiler accepts its syntax" are different claims. */
    {
        char vs[900], fs[700];
        snprintf(vs, sizeof(vs),
                 "#version 330 core\n"
                 "#extension GL_ARB_enhanced_layouts : require\n"
                 "#extension GL_ARB_separate_shader_objects : require\n"
                 "const int base = 1;\n"
                 "layout(location = base + 1) out float value;\n"
                 "void main() {\n%s  value = 0.5;\n}\n", quad_body);
        snprintf(fs, sizeof(fs),
                 "#version 330 core\n"
                 "#extension GL_ARB_enhanced_layouts : require\n"
                 "#extension GL_ARB_separate_shader_objects : require\n"
                 "const int base = 1;\n"
                 "layout(location = base + 1) in float value;\n"
                 "out vec4 colour;\n"
                 "void main() { colour = vec4(value, 0.0, 0.0, 1.0); }\n");
        unsigned char px[4] = {0};
        GLuint prog = have ? link_pair(vs, fs, "const-expr") : 0;
        if (prog) {
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            read_centre(px);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO const-expr got=%u want=128 err=0x%04x\n", px[0],
                (unsigned)error);
        result("const-expr", have && prog && error == GL_NO_ERROR &&
                             px[0] >= 126 && px[0] <= 130);
        if (prog) glDeleteProgram(prog);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
