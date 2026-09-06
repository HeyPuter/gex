/* A persistent mapping is two-way, and the GPU half of it.
 *
 * ARB_buffer_storage lets a buffer stay mapped while the GPU uses it, and GL
 * defines when each side's writes become visible to the other: for the
 * device's writes, MemoryBarrier with CLIENT_MAPPED_BUFFER_BARRIER_BIT.
 * Gallium says the same of PIPE_MAP_COHERENT -- "any writes done by the
 * device are immediately visible to the CPU and vice versa" -- and only the
 * vice versa was ever implemented, in the guest marshaller and in the driver
 * both: the CPU shadow was filled when the buffer was mapped and never again,
 * and every submit uploaded the whole of it back over whatever the shader had
 * written.
 *
 * An atomic counter is the smallest thing that shows it: the shader's
 * increments are the device's writes, and their count is known exactly.
 * KHR-GL44.buffer_storage.map_persistent_draw is the suite's version.
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

static const char *vs_src =
    "#version 430 core\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); gl_PointSize = 1.0; }\n";
static const char *fs_src =
    "#version 430 core\n"
    "layout(binding = 0, offset = 0) uniform atomic_uint counter;\n"
    "out vec4 colour;\n"
    "void main() { atomicCounterIncrement(counter); colour = vec4(1.0); }\n";

static GLuint compile(GLenum stage, const char *src) {
    GLuint s = glCreateShader(stage);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024] = {0}; glGetShaderInfoLog(s, sizeof log, NULL, log);
               fprintf(stderr, "INFO compile failed: %s\n", log); return 0; }
    return s;
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) { fprintf(stderr, "INFO no config\n"); return 3; }
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) {
        fprintf(stderr, "INFO no context\n"); return 3;
    }
    const char *version = (const char *)glGetString(GL_VERSION);
    fprintf(stderr, "INFO version=%s\n", version ? version : "(null)");

    GLuint vs = compile(GL_VERTEX_SHADER, vs_src);
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src);
    if (!vs || !fs) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs); glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint linked = 0; glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0}; glGetProgramInfoLog(prog, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glViewport(0, 0, 16, 16);

    const GLbitfield flags = GL_MAP_READ_BIT | GL_MAP_WRITE_BIT |
                             GL_MAP_PERSISTENT_BIT | GL_MAP_COHERENT_BIT;
    GLuint acb = 0;
    glGenBuffers(1, &acb);
    glBindBuffer(GL_ATOMIC_COUNTER_BUFFER, acb);
    glBufferStorage(GL_ATOMIC_COUNTER_BUFFER, sizeof(GLuint), NULL, flags);
    glBindBufferBase(GL_ATOMIC_COUNTER_BUFFER, 0, acb);
    GLuint *atom = (GLuint *)glMapBufferRange(GL_ATOMIC_COUNTER_BUFFER, 0,
                                              sizeof(GLuint), flags);
    result("map", atom != NULL);
    if (!atom) { printf("SUMMARY cases=%d passed=%d failed=%d\n",
                        cases, passed, cases - passed); return 1; }

    /* The CPU's write has to reach the shader. */
    *atom = 7u;
    glMemoryBarrier(GL_CLIENT_MAPPED_BUFFER_BARRIER_BIT);

    const GLsizei points = 4;
    glUseProgram(prog);
    glEnable(GL_PROGRAM_POINT_SIZE);
    glDrawArrays(GL_POINTS, 0, points);
    glMemoryBarrier(GL_ALL_BARRIER_BITS);
    glFinish();

    /* Seven, plus one per fragment. One point covers one pixel. */
    const GLuint want = 7u + (GLuint)points;
    fprintf(stderr, "INFO through-mapping got=%u want=%u err=0x%04x\n",
            *atom, want, (unsigned)glGetError());
    result("device_writes_visible", *atom == want);

    /* And the same value the ordinary way, which is the control: if this
     * disagrees with the mapping the defect is the mapping, and if they agree
     * and are both wrong the shader never ran. */
    GLuint direct = 0;
    glGetBufferSubData(GL_ATOMIC_COUNTER_BUFFER, 0, sizeof direct, &direct);
    fprintf(stderr, "INFO direct-read got=%u want=%u\n", direct, want);
    result("device_writes_landed", direct == want);

    /* A second round: the CPU writes through the mapping again, and the
     * device must see the new value rather than the one the shadow was
     * uploaded with. */
    *atom = 100u;
    glMemoryBarrier(GL_CLIENT_MAPPED_BUFFER_BARRIER_BIT);
    glDrawArrays(GL_POINTS, 0, points);
    glMemoryBarrier(GL_ALL_BARRIER_BITS);
    glFinish();
    const GLuint want2 = 100u + (GLuint)points;
    fprintf(stderr, "INFO second-round got=%u want=%u\n", *atom, want2);
    result("host_writes_visible", *atom == want2);

    glUnmapBuffer(GL_ATOMIC_COUNTER_BUFFER);
    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
