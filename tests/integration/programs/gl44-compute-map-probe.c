/* KHR-GL44.compute_shader.simple-compute, reproduced call for call.
 *
 * That case, one-work-group and dispatch-indirect all fail in the browser
 * while their own bodies report success, because the only thing that fails is
 * a value comparison that logs nothing: dEQP writes the result into an SSBO,
 * reads it back through a mapping, compares, and returns an error code with
 * no message. The suite then reports "Fail" with an empty reason.
 *
 * gl44-compute-ssbo-probe covers a dispatch read back through a mapping and
 * passes, so the difference is somewhere in the details this keeps exact: a
 * sixteen-byte buffer allocated with no initial data, bound to its indexed
 * point before it is sized, a shader writing floats, the generic binding
 * point cleared in between, and only GL_BUFFER_UPDATE_BARRIER_BIT.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *cs_src =
    "#version 430 core\n"
    "layout(local_size_x = 1, local_size_y = 1) in;\n"
    "layout(std430) buffer Output {\n"
    "  vec4 data;\n"
    "} g_out;\n"
    "void main() {\n"
    "  g_out.data = vec4(1.0, 2.0, 3.0, 4.0);\n"
    "}\n";

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

    /* dEQP's own helper order: create the program, create and attach the
     * shader, flag it for deletion, and only then give it its source. */
    GLuint prog = glCreateProgram();
    GLuint cs = glCreateShader(GL_COMPUTE_SHADER);
    glAttachShader(prog, cs);
    glDeleteShader(cs);
    glShaderSource(cs, 1, &cs_src, NULL);
    glCompileShader(cs);
    GLint ok = 0;
    glGetShaderiv(cs, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024] = {0}; glGetShaderInfoLog(cs, sizeof log, NULL, log);
               fprintf(stderr, "INFO compile failed: %s\n", log); }
    result("deferred_delete_compile", ok == GL_TRUE);
    glLinkProgram(prog);
    GLint linked = 0; glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0}; glGetProgramInfoLog(prog, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log); }
    result("link", linked == GL_TRUE);

    GLint v[3] = { 0, 0, 0 };
    glGetProgramiv(prog, GL_COMPUTE_WORK_GROUP_SIZE, v);
    fprintf(stderr, "INFO work_group_size=%d,%d,%d want=1,1,1\n", v[0], v[1], v[2]);
    result("work_group_size", v[0] == 1 && v[1] == 1 && v[2] == 1);

    /* Bound to its indexed point before it has a size, as the case does. */
    GLuint buffer = 0;
    glGenBuffers(1, &buffer);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, 4 * sizeof(float), NULL,
                 GL_DYNAMIC_DRAW);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, 0);

    glUseProgram(prog);
    glDispatchCompute(1, 1, 1);

    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glMemoryBarrier(GL_BUFFER_UPDATE_BARRIER_BIT);
    const float *data = (const float *)glMapBufferRange(
        GL_SHADER_STORAGE_BUFFER, 0, 4 * sizeof(float), GL_MAP_READ_BIT);
    const int same = data && data[0] == 1.0f && data[1] == 2.0f &&
                     data[2] == 3.0f && data[3] == 4.0f;
    fprintf(stderr, "INFO mapped=%d got=%g,%g,%g,%g want=1,2,3,4 err=0x%04x\n",
            data != NULL, data ? data[0] : 0.0, data ? data[1] : 0.0,
            data ? data[2] : 0.0, data ? data[3] : 0.0, (unsigned)glGetError());
    result("mapped_result", same);
    if (data) glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);

    /* And the same bytes the other way, which says whether the dispatch or
     * the mapping is at fault. */
    float direct[4] = { 0, 0, 0, 0 };
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof direct, direct);
    fprintf(stderr, "INFO direct=%g,%g,%g,%g want=1,2,3,4\n",
            direct[0], direct[1], direct[2], direct[3]);
    result("direct_result", direct[0] == 1.0f && direct[1] == 2.0f &&
                            direct[2] == 3.0f && direct[3] == 4.0f);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
