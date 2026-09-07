/* A bad argument must not destroy the batch.
 *
 * Thirteen generated transport cases treated a negative user-supplied count
 * as a malformed wire packet: they set a.ok = false, which aborts decoding,
 * and the guest then discards every queued byte -- including commands the
 * application had already made that had nothing to do with the bad one, and
 * the PRESENT that ends the frame. The application was told nothing; it saw
 * neither the GL_INVALID_VALUE the spec owes it nor any sign that its other
 * work had evaporated. KHR-GL44.program_interface_query.invalid-value failed
 * for exactly this reason and reported only "Fail".
 *
 * So there are two questions per command, and the second is the important
 * one:
 *
 *   1. does GL raise GL_INVALID_VALUE, as the specification requires?
 *   2. is the connection still carrying commands afterwards?
 *
 * (2) is what a destroyed batch looks like from inside the application, and
 * nothing else in the suite asks it. It is checked by issuing a command whose
 * result comes back from the host and confirming the answer arrives: a
 * discarded batch loses the query and the value the guest reads is whatever
 * was in the reply area before.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>
/* glGetInternalformativ, glGetNamedBufferSubData,
   glInvalidateNamedFramebufferSubData and glVertexArrayVertexBuffers are not
   in Mesa's non-glvnd libGL at all, so linking them directly made this probe
   UNLINKABLE against our own native build -- it could only ever run in the
   browser and against the system's glvnd libGL. */
#include "gl-procs.h"

static int cases, passed;

static GLenum drain(void) {
    GLenum last = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) last = e;
    return last;
}

/* A round trip the host must answer. GL_MAX_TEXTURE_SIZE is a constant the
   host reports and never zero, so a zero here means the reply never came. */
static int link_is_alive(void) {
    GLint probe = 0;
    glGetIntegerv(GL_MAX_TEXTURE_SIZE, &probe);
    (void)drain();
    return probe > 0;
}

/* Both halves, named separately so a log line says which one failed.
 *
 * "An error, any error" rather than GL_INVALID_VALUE specifically. The
 * specification says INVALID_VALUE for a negative count throughout, and
 * native Mesa answers GL_INVALID_OPERATION for all four of the multi-bind
 * entry points. Which of them is right is not what this probe is for, and the
 * reference is the authority for what this driver should do -- it is the same
 * Mesa. The bug being tested for produced *no* error at all, so this still
 * catches it, and the second half catches it far more loudly. */
static void check_negative(const char *name, GLenum got) {
    const int raised = got != GL_NO_ERROR;
    const int alive = link_is_alive();
    ++cases; if (raised) ++passed;
    fprintf(stderr, "INFO %-40s got=0x%04x want=any error alive=%d\n", name,
            (unsigned)got, alive);
    printf("CASE %s_raises %s\n", name, raised ? "PASS" : "FAIL");
    ++cases; if (alive) ++passed;
    printf("CASE %s_batch_survives %s\n", name, alive ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 430 core\n"
    "layout(location = 0) in vec4 pos;\n"
    "out float carried;\n"
    "void main() { carried = pos.x; gl_Position = pos; }\n";
static const char *fs_src =
    "#version 430 core\n"
    "in float carried;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(carried); }\n";

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    if (gex_load_procs() != 0) {
        fprintf(stderr, "INFO entry points unresolved; not measuring\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 3;
    }

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs); glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint linked = 0; glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0};
                   glGetProgramInfoLog(prog, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }

    GLuint buffer = 0; glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    const float data[8] = { 0.f, 0.f, 1.f, 0.f, 0.f, 1.f, 1.f, 1.f };
    glBufferData(GL_ARRAY_BUFFER, sizeof data, data, GL_STATIC_DRAW);
    drain();

    /* The multi-bind family: glBindBuffersBase, glBindImageTextures,
       glBindSamplers, glBindTextures. */
    glBindBuffersBase(GL_UNIFORM_BUFFER, 0, -1, NULL);
    check_negative("bind_buffers_base", drain());
    glBindImageTextures(0, -1, NULL);
    check_negative("bind_image_textures", drain());
    glBindSamplers(0, -1, NULL);
    check_negative("bind_samplers", drain());
    glBindTextures(0, -1, NULL);
    check_negative("bind_textures", drain());

    /* glGetBufferSubData: a negative size, and a negative offset. */
    char readback[16];
    memset(readback, 0, sizeof readback);
    glGetBufferSubData(GL_ARRAY_BUFFER, 0, -4, readback);
    check_negative("get_buffer_sub_data_size", drain());
    glGetBufferSubData(GL_ARRAY_BUFFER, -4, 4, readback);
    check_negative("get_buffer_sub_data_offset", drain());

    /* The internal-format queries. */
    GLint formats[4] = {0};
    glGetInternalformativ(GL_TEXTURE_2D, GL_RGBA8, GL_SAMPLES, -1, formats);
    check_negative("get_internalformativ", drain());
    /* glGetInternalformati64v is deliberately absent. A negative bufSize
       overruns a stack array inside Mesa itself before the call that would
       have validated it -- see gl44-internalformat-negative-probe, which
       cannot use the stock reference because the reference segfaults. */

    /* glGetProgramResourceiv, which is where the CTS found this. */
    const GLenum props[1] = { GL_TYPE };
    GLint values[4] = {0};
    GLsizei length = 0;
    glGetProgramResourceiv(prog, GL_PROGRAM_INPUT, 0, -1, props,
                           (GLsizei)(sizeof values / sizeof *values), &length, values);
    check_negative("get_program_resourceiv_propcount", drain());
    glGetProgramResourceiv(prog, GL_PROGRAM_INPUT, 0, 1, props, -1, &length, values);
    check_negative("get_program_resourceiv_bufsize", drain());

    /* The multi-draw family. glMultiDrawArrays already behaved; it is here so
       that a regression in the shape the others were made to copy shows up
       next to them. */
    glMultiDrawArrays(GL_TRIANGLES, NULL, NULL, -1);
    check_negative("multi_draw_arrays", drain());
    glMultiDrawElements(GL_TRIANGLES, NULL, GL_UNSIGNED_INT, NULL, -1);
    check_negative("multi_draw_elements", drain());
    glMultiDrawElementsBaseVertex(GL_TRIANGLES, NULL, GL_UNSIGNED_INT, NULL, -1, NULL);
    check_negative("multi_draw_elements_base_vertex", drain());

    /* glTransformFeedbackVaryings. */
    glTransformFeedbackVaryings(prog, -1, NULL, GL_SEPARATE_ATTRIBS);
    check_negative("transform_feedback_varyings", drain());

    /* The same defect in the hand-written half of the transport. These four
       are not generated, and each had it at both layers independently -- the
       guest returning without sending, and the host refusing the batch. */
    GLuint fbo = 0; glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glInvalidateSubFramebuffer(GL_FRAMEBUFFER, -1, NULL, 0, 0, 4, 4);
    check_negative("invalidate_sub_framebuffer", drain());
    glInvalidateNamedFramebufferSubData(fbo, -1, NULL, 0, 0, 4, 4);
    check_negative("invalidate_named_framebuffer_sub_data", drain());
    glBindFramebuffer(GL_FRAMEBUFFER, 0);

    glGetNamedBufferSubData(buffer, 0, -4, readback);
    check_negative("get_named_buffer_sub_data", drain());

    GLuint vaobj = 0; glGenVertexArrays(1, &vaobj);
    glBindVertexArray(vaobj);
    glVertexArrayVertexBuffers(vaobj, 0, -1, NULL, NULL, NULL);
    check_negative("vertex_array_vertex_buffers", drain());
    glBindVertexArray(0);

    /* The five the guest answers for itself. It joins the caller's shader
       fragments and uniform-name arrays into one string, and handles
       glDebugMessageControl entirely locally, so the host never sees the count
       the application passed -- there is nothing to forward. These latch
       GL_INVALID_VALUE in the guest, which is safe only because the reference
       agrees: native Mesa raises exactly that for all five.

       The four robustness readbacks are deliberately absent, and this is the
       reason: native Mesa raises *nothing* for a negative bufSize on
       glReadnPixels, glGetnTexImage, glGetnCompressedTexImage and
       glGetTextureSubImage. The guest's silence already matches the reference
       there, so there was never a defect to fix, and asserting an error would
       be asserting against the only authority available. */
    GLuint sh = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(sh, -1, &vs_src, NULL);
    check_negative("shader_source", drain());
    GLuint shader_names[1] = { sh };
    char binary_scratch[64];
    glShaderBinary(-1, shader_names, 0, binary_scratch, 0);
    check_negative("shader_binary_count", drain());
    glShaderBinary(1, shader_names, 0, binary_scratch, -1);
    check_negative("shader_binary_length", drain());

    GLint binary_length = 0; GLenum binary_format = 0;
    glGetProgramBinary(prog, -1, &binary_length, &binary_format, binary_scratch);
    check_negative("get_program_binary", drain());

    const char *uniform_names[1] = { "scale" };
    GLuint uniform_indices[1] = { 0 };
    glGetUniformIndices(prog, -1, uniform_names, uniform_indices);
    check_negative("get_uniform_indices", drain());

    GLuint debug_ids[1] = { 0 };
    glDebugMessageControl(GL_DONT_CARE, GL_DONT_CARE, GL_DONT_CARE, -1,
                          debug_ids, GL_TRUE);
    check_negative("debug_message_control", drain());

    /* Having survived all of that, the connection must still be able to do
       real work -- not merely answer a query. A draw that reads the buffer
       bound before the first bad call proves the state also survived. */
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glEnableVertexAttribArray(0);
    glUseProgram(prog);
    glClearColor(0.f, 0.f, 0.f, 1.f); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    GLenum draw_error = drain();
    unsigned char pixel[4] = {0};
    glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    const int drew = draw_error == GL_NO_ERROR && pixel[0] > 0;
    ++cases; if (drew) ++passed;
    fprintf(stderr, "INFO %-40s error=0x%04x pixel=%u\n",
            "draw_after_negative_counts", (unsigned)draw_error, pixel[0]);
    printf("CASE draw_after_negative_counts %s\n", drew ? "PASS" : "FAIL");

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
