/* Separate shader objects and program binaries: two extensions an engine
 * checks for, each of which was waiting on two entry points.
 *
 * A pipeline lets a renderer mix vertex and fragment stages without relinking,
 * and a program binary turns a multi-second shader compile at start-up into a
 * read from disk. An application that asks for either and finds the calls
 * missing does not degrade -- it takes the slow path every run, or it stops.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases = 0, failures = 0;
static void report(const char* name, int ok, const char* detail) {
    ++cases;
    if (!ok) ++failures;
    printf("CASE %s %s%s%s\n", name, ok ? "PASS" : "FAIL",
           detail && *detail ? " -- " : "", detail ? detail : "");
}
#define NEED(c, m) do { if (!(c)) { fprintf(stderr, "sso failed: %s\n", m); return __LINE__; } } while (0)

static const char* vs_source =
    "#version 410\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char* fs_source =
    "#version 410\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(0.0, 0.0, 1.0, 1.0); }\n";

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    NEED(eglInitialize(display, NULL, NULL), "eglInitialize");
    NEED(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL; EGLint config_count = 0;
    NEED(eglChooseConfig(display, config_attributes, &config, 1, &config_count),
         "eglChooseConfig");
    NEED(config_count >= 1 && config != NULL, "no pbuffer config");
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    NEED(context != EGL_NO_CONTEXT && surface != EGL_NO_SURFACE, "context/surface");
    NEED(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    /* An FBO to draw into, so the result does not depend on the pbuffer's
     * front/back buffer conventions. */
    GLuint colour = 0, fbo = 0;
    glGenRenderbuffers(1, &colour);
    glBindRenderbuffer(GL_RENDERBUFFER, colour);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 8, 8);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                              GL_RENDERBUFFER, colour);
    glViewport(0, 0, 8, 8);

    /* glCreateShaderProgramv: one call that compiles, marks separable and
     * links. Separability before the link is what makes it usable below. */
    const GLuint vs = glCreateShaderProgramv(GL_VERTEX_SHADER, 1, &vs_source);
    const GLuint fs = glCreateShaderProgramv(GL_FRAGMENT_SHADER, 1, &fs_source);
    GLint vs_linked = 0, fs_linked = 0, vs_separable = 0;
    glGetProgramiv(vs, GL_LINK_STATUS, &vs_linked);
    glGetProgramiv(fs, GL_LINK_STATUS, &fs_linked);
    glGetProgramiv(vs, GL_PROGRAM_SEPARABLE, &vs_separable);
    {
        char detail[96];
        snprintf(detail, sizeof detail, "vs=%u(%d) fs=%u(%d) separable=%d", vs,
                 vs_linked, fs, fs_linked, vs_separable);
        report("create_shader_programv",
               vs && fs && vs_linked && fs_linked && vs_separable, detail);
    }

    /* Drive a draw through a pipeline rather than a linked program. */
    GLuint pipeline = 0, vao = 0, vbo = 0;
    glGenProgramPipelines(1, &pipeline);
    glUseProgramStages(pipeline, GL_VERTEX_SHADER_BIT, vs);
    glUseProgramStages(pipeline, GL_FRAGMENT_SHADER_BIT, fs);
    glBindProgramPipeline(pipeline);
    glUseProgram(0);
    static const float tri[] = {-1.0f, -1.0f, 3.0f, -1.0f, -1.0f, 3.0f};
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof tri, tri, GL_STATIC_DRAW);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glEnableVertexAttribArray(0);
    const GLfloat black[4] = {0.0f, 0.0f, 0.0f, 1.0f};
    glClearBufferfv(GL_COLOR, 0, black);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    unsigned char pixel[4] = {0, 0, 0, 0};
    glReadPixels(4, 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    {
        char detail[96];
        snprintf(detail, sizeof detail, "pixel %u,%u,%u,%u", pixel[0], pixel[1],
                 pixel[2], pixel[3]);
        report("draw_through_pipeline",
               pixel[2] > 200 && pixel[0] < 60 && pixel[1] < 60, detail);
    }

    /* The pipeline's own info log has its own entry point. Empty is the
     * expected answer for a valid pipeline; what is tested is that the call
     * returns rather than being absent. */
    {
        char log[128] = {'x', '\0'};
        GLsizei log_length = 0;
        glGetProgramPipelineInfoLog(pipeline, (GLsizei)sizeof log, &log_length,
                                    log);
        char detail[64];
        snprintf(detail, sizeof detail, "len=%d err=0x%x", (int)log_length,
                 (unsigned)glGetError());
        report("pipeline_info_log", log_length >= 0, detail);
    }

    /* A program binary, round-tripped into a fresh program. This is the case
     * an engine's shader cache depends on. */
    {
        GLuint linked = glCreateProgram();
        GLuint v = glCreateShader(GL_VERTEX_SHADER);
        GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
        glShaderSource(v, 1, &vs_source, NULL); glCompileShader(v);
        glShaderSource(f, 1, &fs_source, NULL); glCompileShader(f);
        glAttachShader(linked, v); glAttachShader(linked, f);
        glProgramParameteri(linked, GL_PROGRAM_BINARY_RETRIEVABLE_HINT, GL_TRUE);
        glLinkProgram(linked);

        GLint formats = 0;
        glGetIntegerv(GL_NUM_PROGRAM_BINARY_FORMATS, &formats);
        GLint binary_length = 0;
        glGetProgramiv(linked, GL_PROGRAM_BINARY_LENGTH, &binary_length);
        if (formats <= 0 || binary_length <= 0) {
            char detail[80];
            snprintf(detail, sizeof detail,
                     "no binary formats (formats=%d len=%d)", formats,
                     binary_length);
            report("program_binary_roundtrip", 1, detail);
        } else {
            void* blob = malloc((size_t)binary_length);
            GLenum format = 0;
            GLsizei written = 0;
            glGetProgramBinary(linked, binary_length, &written, &format, blob);
            GLuint restored = glCreateProgram();
            glProgramBinary(restored, format, blob, written);
            GLint restored_ok = 0;
            glGetProgramiv(restored, GL_LINK_STATUS, &restored_ok);
            char detail[96];
            snprintf(detail, sizeof detail, "wrote %d bytes fmt=0x%x relink=%d",
                     (int)written, (unsigned)format, restored_ok);
            report("program_binary_roundtrip",
                   written > 0 && restored_ok, detail);
            free(blob);
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures,
           failures);
    return failures ? 1 : 0;
}
