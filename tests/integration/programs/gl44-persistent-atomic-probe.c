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

    /* The same again, but with the mapping the suite's draw case actually
     * uses on a core context: persistent and explicitly flushed rather than
     * coherent, over storage that carries no GL_DYNAMIC_STORAGE_BIT. A
     * compatibility context takes the coherent branch instead, so a native
     * run of KHR-GL44.buffer_storage.map_persistent_draw never reaches this
     * at all -- the browser gets a core profile and does. */
    {
        GLuint fe = 0;
        GLuint *fe_map = NULL;
        const GLbitfield fe_storage = GL_MAP_PERSISTENT_BIT | GL_MAP_READ_BIT |
                                      GL_MAP_WRITE_BIT;
        const GLbitfield fe_access = fe_storage | GL_MAP_FLUSH_EXPLICIT_BIT;
        glGenBuffers(1, &fe);
        glBindBuffer(GL_ATOMIC_COUNTER_BUFFER, fe);
        glBufferStorage(GL_ATOMIC_COUNTER_BUFFER, sizeof(GLuint), NULL,
                        fe_storage);
        glBindBufferBase(GL_ATOMIC_COUNTER_BUFFER, 0, fe);
        fe_map = (GLuint *)glMapBufferRange(GL_ATOMIC_COUNTER_BUFFER, 0,
                                            sizeof(GLuint), fe_access);
        GLenum fe_err = glGetError();
        result("flush_explicit_map", fe_map != NULL && fe_err == GL_NO_ERROR);
        if (fe_map) {
            *fe_map = 20u;
            glFlushMappedBufferRange(GL_ATOMIC_COUNTER_BUFFER, 0,
                                     sizeof(GLuint));
            glDrawArrays(GL_POINTS, 0, points);
            glMemoryBarrier(GL_ALL_BARRIER_BITS);
            glFinish();
        }
        GLuint fe_direct = 0;
        glGetBufferSubData(GL_ATOMIC_COUNTER_BUFFER, 0, sizeof fe_direct,
                           &fe_direct);
        const GLuint fe_want = 20u + (GLuint)points;
        fprintf(stderr, "INFO flush-explicit map=%d through=%u direct=%u "
                "want=%u err=0x%04x\n", fe_map != NULL,
                fe_map ? *fe_map : 0, fe_direct, fe_want, (unsigned)fe_err);
        result("flush_explicit_reaches_device", fe_direct == fe_want);
        result("flush_explicit_visible", fe_map && *fe_map == fe_want);
        if (fe_map) glUnmapBuffer(GL_ATOMIC_COUNTER_BUFFER);
        if (fe) glDeleteBuffers(1, &fe);
        while (glGetError() != GL_NO_ERROR) {}
    }

    /* A write-only persistent uniform buffer, explicitly flushed: the shape
     * the suite's draw case gives its "rectangles" buffer, and the one thing
     * about that case not covered above. Write-only matters -- the shadow is
     * never filled from the device for such a mapping unless it is
     * persistent, which is the seeding this probe's dispatch case caught. */
    {
        GLuint ubo = 0;
        GLuint pattern[4] = { 0x11111111u, 0x22222222u, 0x33333333u,
                              0x44444444u };
        GLuint *ubo_map = NULL;
        GLuint readback[4] = { 0, 0, 0, 0 };
        glGenBuffers(1, &ubo);
        glBindBuffer(GL_UNIFORM_BUFFER, ubo);
        glBufferStorage(GL_UNIFORM_BUFFER, sizeof pattern, NULL,
                        GL_MAP_PERSISTENT_BIT | GL_MAP_WRITE_BIT);
        glBindBuffer(GL_UNIFORM_BUFFER, 0);
        glBindBufferBase(GL_UNIFORM_BUFFER, 0, ubo);
        ubo_map = (GLuint *)glMapBufferRange(
            GL_UNIFORM_BUFFER, 0, sizeof pattern,
            GL_MAP_PERSISTENT_BIT | GL_MAP_WRITE_BIT |
            GL_MAP_FLUSH_EXPLICIT_BIT);
        if (ubo_map) {
            memcpy(ubo_map, pattern, sizeof pattern);
            glFlushMappedBufferRange(GL_UNIFORM_BUFFER, 0, sizeof pattern);
        }
        glGetBufferSubData(GL_UNIFORM_BUFFER, 0, sizeof readback, readback);
        const int same = memcmp(readback, pattern, sizeof pattern) == 0;
        fprintf(stderr, "INFO ubo-flush map=%d got=%08x,%08x want=%08x,%08x "
                "err=0x%04x\n", ubo_map != NULL, readback[0], readback[1],
                pattern[0], pattern[1], (unsigned)glGetError());
        result("uniform_write_only_flush", ubo_map != NULL && same);
        if (ubo_map) glUnmapBuffer(GL_UNIFORM_BUFFER);
        if (ubo) glDeleteBuffers(1, &ubo);
        while (glGetError() != GL_NO_ERROR) {}
    }

    /* And the same two directions across a compute dispatch rather than a
     * draw, which is what KHR-GL44.buffer_storage.map_persistent_dispatch
     * asks: a source mapped write-only and a destination mapped read-only,
     * both persistent and coherent, with nothing but glFinish between the
     * dispatch and the read. */
    {
        static const char *cs_src =
            "#version 430 core\n"
            "layout(local_size_x = 1) in;\n"
            "layout(binding = 0, std430) readonly buffer Source { uint src[]; };\n"
            "layout(binding = 1, std430) writeonly buffer Dest { uint dst[]; };\n"
            "void main() { dst[gl_GlobalInvocationID.x] =\n"
            "                  src[gl_GlobalInvocationID.x] + 1u; }\n";
        enum { COUNT = 16 };
        GLuint initial[COUNT];
        for (unsigned i = 0; i < COUNT; ++i) initial[i] = i;

        GLuint cs = compile(GL_COMPUTE_SHADER, cs_src);
        GLuint cprog = 0;
        if (cs) {
            cprog = glCreateProgram();
            glAttachShader(cprog, cs);
            glLinkProgram(cprog);
            GLint ok = 0; glGetProgramiv(cprog, GL_LINK_STATUS, &ok);
            if (!ok) { char log[1024] = {0};
                       glGetProgramInfoLog(cprog, sizeof log, NULL, log);
                       fprintf(stderr, "INFO compute link failed: %s\n", log);
                       cprog = 0; }
        }
        if (cprog) {
            GLuint src_buf = 0, dst_buf = 0;
            glGenBuffers(1, &src_buf);
            glBindBuffer(GL_SHADER_STORAGE_BUFFER, src_buf);
            glBufferStorage(GL_SHADER_STORAGE_BUFFER, sizeof initial, initial,
                            GL_MAP_COHERENT_BIT | GL_MAP_PERSISTENT_BIT |
                            GL_MAP_WRITE_BIT);
            GLuint *src_map = (GLuint *)glMapBufferRange(
                GL_SHADER_STORAGE_BUFFER, 0, sizeof initial,
                GL_MAP_COHERENT_BIT | GL_MAP_PERSISTENT_BIT | GL_MAP_WRITE_BIT);

            glGenBuffers(1, &dst_buf);
            glBindBuffer(GL_SHADER_STORAGE_BUFFER, dst_buf);
            glBufferStorage(GL_SHADER_STORAGE_BUFFER, sizeof initial, NULL,
                            GL_MAP_COHERENT_BIT | GL_MAP_PERSISTENT_BIT |
                            GL_MAP_READ_BIT);
            GLuint *dst_map = (GLuint *)glMapBufferRange(
                GL_SHADER_STORAGE_BUFFER, 0, sizeof initial,
                GL_MAP_COHERENT_BIT | GL_MAP_PERSISTENT_BIT | GL_MAP_READ_BIT);

            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, src_buf);
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, dst_buf);
            glUseProgram(cprog);
            glDispatchCompute(COUNT, 1, 1);
            glFinish();

            int first_ok = dst_map != NULL;
            for (unsigned i = 0; first_ok && i < COUNT; ++i)
                if (dst_map[i] != i + 1u) first_ok = 0;
            fprintf(stderr, "INFO dispatch first dst[0..3]=%u,%u,%u,%u "
                    "want=1,2,3,4 err=0x%04x\n",
                    dst_map ? dst_map[0] : 0, dst_map ? dst_map[1] : 0,
                    dst_map ? dst_map[2] : 0, dst_map ? dst_map[3] : 0,
                    (unsigned)glGetError());
            result("dispatch_result_visible", first_ok);

            /* Now change the source through its mapping and dispatch again. */
            int second_ok = src_map != NULL && dst_map != NULL;
            if (src_map)
                for (unsigned i = 0; i < COUNT; ++i) src_map[i] = 100u + i;
            glDispatchCompute(COUNT, 1, 1);
            glFinish();
            for (unsigned i = 0; second_ok && i < COUNT; ++i)
                if (dst_map[i] != 101u + i) second_ok = 0;
            fprintf(stderr, "INFO dispatch second dst[0..3]=%u,%u,%u,%u "
                    "want=101,102,103,104\n",
                    dst_map ? dst_map[0] : 0, dst_map ? dst_map[1] : 0,
                    dst_map ? dst_map[2] : 0, dst_map ? dst_map[3] : 0);
            result("dispatch_source_update", second_ok);

            if (src_map) { glBindBuffer(GL_SHADER_STORAGE_BUFFER, src_buf);
                           glUnmapBuffer(GL_SHADER_STORAGE_BUFFER); }
            if (dst_map) { glBindBuffer(GL_SHADER_STORAGE_BUFFER, dst_buf);
                           glUnmapBuffer(GL_SHADER_STORAGE_BUFFER); }
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
