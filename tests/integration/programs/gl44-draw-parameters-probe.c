/* ARB_shader_draw_parameters -- the capability Blender names.
 *
 * Three values a vertex shader can read about the draw that invoked it:
 * `gl_BaseVertexARB` (the basevertex parameter), `gl_BaseInstanceARB` (the
 * baseinstance parameter), and `gl_DrawIDARB` (which sub-draw of a
 * glMultiDraw* command this is). A renderer that packs many meshes into one
 * vertex buffer and draws them with one command needs all three to find its
 * own data; Blender 5.2 refuses to start without the extension, and it is
 * core GL 4.6.
 *
 * WGSL has no builtin for any of them. `instance_index` counts from
 * firstInstance -- which this driver folds into the vertex buffer offsets, so
 * it is not even visible there -- and there is nothing at all for the base
 * vertex or the draw index. They are per-draw scalars the driver knows, so
 * they have to be *delivered*, and the cases below are chosen so that
 * delivering the wrong one is visible:
 *
 *   `base-vertex`    glDrawElementsBaseVertex with basevertex = 7
 *   `base-vertex-0`  plain glDrawArrays, where gl_BaseVertexARB must be
 *                    **zero** -- the specification says so explicitly for a
 *                    command with no basevertex parameter, and the obvious
 *                    implementation reports `first` instead
 *   `base-instance`  glDrawArraysInstancedBaseInstance with baseinstance = 5
 *   `draw-id`        glMultiDrawArrays with three sub-draws, each covering a
 *                    different third of the viewport, each reporting its own
 *                    index
 *   `vertex-id`      gl_VertexID alongside gl_BaseVertexARB in one shader,
 *                    because the internal "first vertex" value the driver
 *                    derives gl_VertexID from is a *different* number from
 *                    gl_BaseVertexARB and sharing one word for both is the
 *                    mistake this case catches
 *
 * Each value is written to the red channel divided by 255, so the readback is
 * the integer itself.
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

#define SIDE 24

/* The quad is built from gl_VertexID so no attribute is needed; `expr` is
 * whatever integer the case wants reported. */
static GLuint build(const char *expr, const char *extra_out)
{
    char vs_src[900];
    snprintf(vs_src, sizeof(vs_src),
             "#version 330 core\n"
             "#extension GL_ARB_shader_draw_parameters : require\n"
             "flat out int reported;\n"
             "%s\n"
             "uniform int corner;\n"
             "void main() {\n"
             "  int v = gl_VertexID - corner;\n"
             "  vec2 p = vec2((v & 1) == 0 ? -1.0 : 1.0,\n"
             "                (v & 2) == 0 ? -1.0 : 1.0);\n"
             "  gl_Position = vec4(p, 0.0, 1.0);\n"
             "  reported = %s;\n"
             "}\n", extra_out ? extra_out : "", expr);
    static const char *fs_src =
        "#version 330 core\n"
        "flat in int reported;\n"
        "out vec4 colour;\n"
        "void main() {\n"
        "  colour = vec4(float(reported) / 255.0, 0.0, 0.0, 1.0);\n"
        "}\n";
    const char *vs_ptr = vs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_ptr, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(vs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO vs compile: %s\n", log);
        glDeleteShader(vs);
        return 0;
    }
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) { glDeleteShader(vs); glDeleteShader(fs); return 0; }
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
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

static unsigned centre(void)
{
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

    const int have = have_extension("GL_ARB_shader_draw_parameters");
    fprintf(stderr, "INFO version=%s draw_parameters=%d\n",
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

    /* Case 1: basevertex through glDrawElementsBaseVertex. The index list
     * names 0..3 and the base shifts them, so `corner` compensates and the
     * quad still covers the viewport. */
    {
        unsigned got = 0xffff;
        GLuint prog = have ? build("gl_BaseVertexARB", NULL) : 0;
        GLuint ibo = 0;
        if (prog) {
            const uint32_t idx[4] = {0, 1, 2, 3};
            glGenBuffers(1, &ibo);
            glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo);
            glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(idx), idx,
                         GL_STATIC_DRAW);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "corner"), 7);
            glDrawElementsBaseVertex(GL_TRIANGLE_STRIP, 4, GL_UNSIGNED_INT,
                                     NULL, 7);
            got = centre();
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO base-vertex got=%u want=7 err=0x%04x\n", got,
                (unsigned)error);
        result("base-vertex", have && prog && error == GL_NO_ERROR && got == 7);
        if (prog) glDeleteProgram(prog);
        if (ibo) glDeleteBuffers(1, &ibo);
        glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, 0);
    }

    /* Case 2: glDrawArrays, where the specification says gl_BaseVertexARB is
     * zero however the draw starts. */
    {
        unsigned got = 0xffff;
        GLuint prog = have ? build("gl_BaseVertexARB + 1", NULL) : 0;
        if (prog) {
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "corner"), 9);
            glDrawArrays(GL_TRIANGLE_STRIP, 9, 4);
            got = centre();
        }
        const GLenum error = glGetError();
        /* +1 so that "drew nothing" and "reported zero" are different
         * answers: an untouched pixel reads 0 and a correct one reads 1. */
        fprintf(stderr, "INFO base-vertex-0 got=%u want=1 err=0x%04x\n", got,
                (unsigned)error);
        result("base-vertex-0",
               have && prog && error == GL_NO_ERROR && got == 1);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 3: baseinstance. */
    {
        unsigned got = 0xffff;
        GLuint prog = have ? build("gl_BaseInstanceARB", NULL) : 0;
        if (prog) {
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "corner"), 0);
            glDrawArraysInstancedBaseInstance(GL_TRIANGLE_STRIP, 0, 4, 1, 5);
            got = centre();
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO base-instance got=%u want=5 err=0x%04x\n", got,
                (unsigned)error);
        result("base-instance",
               have && prog && error == GL_NO_ERROR && got == 5);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 4: the draw index within a glMultiDrawArrays. Each sub-draw is a
     * full-viewport quad reporting its own index, so the last one wins the
     * centre pixel -- which is enough to tell 2 from 0 and from a driver that
     * gives every sub-draw the same number. Checked at all three indices by
     * running the command three times with a growing count. */
    {
        int ok = have;
        unsigned seen[3] = {0xffff, 0xffff, 0xffff};
        GLuint prog = have ? build("gl_DrawIDARB + 1", NULL) : 0;
        for (unsigned draws = 1; prog && draws <= 3; ++draws) {
            const GLint first[3] = {0, 0, 0};
            const GLsizei count[3] = {4, 4, 4};
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "corner"), 0);
            glMultiDrawArrays(GL_TRIANGLE_STRIP, first, count, (GLsizei)draws);
            seen[draws - 1] = centre();
            /* The last sub-draw painted over the rest, so the centre carries
             * its index plus one. */
            if (seen[draws - 1] != draws) ok = 0;
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO draw-id got=%u,%u,%u want=1,2,3 err=0x%04x\n",
                seen[0], seen[1], seen[2], (unsigned)error);
        result("draw-id", ok && prog && error == GL_NO_ERROR);
        if (prog) glDeleteProgram(prog);
    }

    /* Case 5: gl_VertexID and gl_BaseVertexARB in one shader. The internal
     * first-vertex value gl_VertexID is derived from is a different number
     * from gl_BaseVertexARB, and a driver that delivers one word for both
     * gets this wrong while passing case 1. */
    {
        unsigned got = 0xffff;
        GLuint prog = have
            ? build("gl_VertexID * 10 + gl_BaseVertexARB", NULL) : 0;
        if (prog) {
            const uint32_t idx[4] = {0, 1, 2, 3};
            GLuint ibo = 0;
            glGenBuffers(1, &ibo);
            glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo);
            glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(idx), idx,
                         GL_STATIC_DRAW);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "corner"), 3);
            /* Base 3 over indices 0..3, so gl_VertexID is 3..6 and the flat
             * varying takes the provoking vertex's -- whichever that is, the
             * value is 10*id + 3 and the tens digit says which. */
            glDrawElementsBaseVertex(GL_TRIANGLE_STRIP, 4, GL_UNSIGNED_INT,
                                     NULL, 3);
            got = centre();
            glDeleteBuffers(1, &ibo);
            glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, 0);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO vertex-id got=%u want=%%10==3 and >=30 "
                "err=0x%04x\n", got, (unsigned)error);
        result("vertex-id", have && prog && error == GL_NO_ERROR &&
                            got != 0xffff && got % 10 == 3 && got >= 30);
        if (prog) glDeleteProgram(prog);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
