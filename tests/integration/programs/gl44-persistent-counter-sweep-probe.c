/* A GPU-incremented atomic counter through a persistent map, swept over the
 * draw size.
 *
 * gl44-persistent-atomic-probe next to this one asks whether each direction of
 * a persistent mapping works at all. This one asks whether the count is
 * *right*, at 1, 2, 3, 4, 8 and 16 points, under both ways an application may
 * publish a write to such a mapping -- MemoryBarrier, and
 * GL_MAP_FLUSH_EXPLICIT_BIT with an explicit glFlushMappedBufferRange, which
 * is what KHR-GL44.buffer_storage.map_persistent_draw uses on a 4.5-or-better
 * context.
 *
 * The sweep is the point. The counts came back 2, 4, 7, 11, 19 and 35, whose
 * *deltas* are exactly the right answers: nothing was over-invoking, the
 * counter was never being reset, and the guest's write through the mapping was
 * not reaching the host. A single count -- 3 where 2 was wanted -- reads
 * instead as the emulated geometry stage running an extra invocation, which is
 * the wrong place to go looking. Hence a curve rather than a verdict.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases, passed;

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-40s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 440 core\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
/* Increments in the fragment stage: the simplest place an atomic counter can
   be touched, so a failure here is the counter and not the stage. */
static const char *fs_atomic_src =
    "#version 440 core\n"
    "layout(binding = 0) uniform atomic_uint counter;\n"
    "out vec4 colour;\n"
    "void main() {\n"
    "    atomicCounterIncrement(counter);\n"
    "    colour = vec4(0.0, 1.0, 0.0, 1.0);\n"
    "}\n";
/* And in a geometry shader, which is the CTS's own shape. */
static const char *gs_atomic_src =
    "#version 440 core\n"
    "layout(points) in;\n"
    "layout(triangle_strip, max_vertices = 4) out;\n"
    "layout(binding = 0) uniform atomic_uint counter;\n"
    "void main() {\n"
    "    atomicCounterIncrement(counter);\n"
    "    gl_Position = vec4(-1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4(-1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4( 1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4( 1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
    "}\n";
static const char *fs_plain_src =
    "#version 440 core\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(0.0, 1.0, 0.0, 1.0); }\n";

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024] = {0};
               glGetShaderInfoLog(sh, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s compile failed: %s\n", what, log); }
    return ok ? sh : 0;
}

static GLuint link2(GLuint a, GLuint b, GLuint c) {
    GLuint p = glCreateProgram();
    glAttachShader(p, a); glAttachShader(p, b);
    if (c) glAttachShader(p, c);
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) { char log[1024] = {0};
               glGetProgramInfoLog(p, sizeof log, NULL, log);
               fprintf(stderr, "INFO link failed: %s\n", log);
               return 0; }
    return p;
}

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

    GLuint vs = compile(GL_VERTEX_SHADER, vs_src, "vs");
    GLuint fs_atomic = compile(GL_FRAGMENT_SHADER, fs_atomic_src, "fs atomic");
    GLuint fs_plain = compile(GL_FRAGMENT_SHADER, fs_plain_src, "fs plain");
    GLuint gs = compile(GL_GEOMETRY_SHADER, gs_atomic_src, "gs atomic");
    if (!vs || !fs_atomic || !fs_plain || !gs) {
        printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glViewport(0, 0, 16, 16);

    /* An immutable, persistently mapped, readable atomic counter buffer --
       exactly the CTS's storage and map flags. */
    GLuint atom = 0; glGenBuffers(1, &atom);
    glBindBuffer(GL_ATOMIC_COUNTER_BUFFER, atom);
    const GLuint zero = 0;
    glBufferStorage(GL_ATOMIC_COUNTER_BUFFER, sizeof(GLuint), &zero,
                    GL_MAP_PERSISTENT_BIT | GL_MAP_READ_BIT |
                    GL_MAP_WRITE_BIT);
    eq("buffer_storage_is_clean", glGetError(), GL_NO_ERROR);
    GLuint *map = (GLuint *)glMapBufferRange(
        GL_ATOMIC_COUNTER_BUFFER, 0, sizeof(GLuint),
        GL_MAP_PERSISTENT_BIT | GL_MAP_READ_BIT | GL_MAP_WRITE_BIT);
    eq("persistent_map_succeeded", map != NULL, 1);
    if (!map) { printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
                       cases - passed); return 1; }
    glBindBufferBase(GL_ATOMIC_COUNTER_BUFFER, 0, atom);

    /* Seed it through the mapping and push that to the host, as the CTS does. */
    *map = 0;
    glMemoryBarrier(GL_CLIENT_MAPPED_BUFFER_BARRIER_BIT);

    /* One fragment-stage draw. The counter should end up at 256 for a full
       16x16 target, but the exact figure is not the point -- nonzero is. */
    GLuint program = link2(vs, fs_atomic, 0);
    eq("fragment_atomic_program_links", program != 0, 1);
    if (program) {
        glUseProgram(program);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f); glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_POINTS, 0, 1);
        glMemoryBarrier(GL_ALL_BARRIER_BITS);
        /* First the plain read-back, which does not involve the mapping. */
        GLuint direct = 0;
        glGetBufferSubData(GL_ATOMIC_COUNTER_BUFFER, 0, sizeof direct,
                           &direct);
        eq("fragment_counter_incremented", direct > 0, 1);
        /* Then the same value through the persistent mapping. */
        eq("persistent_map_sees_fragment_write", *map == direct, 1);
        fprintf(stderr, "INFO direct=%u map=%u\n", direct, *map);
    }

    /* And the CTS's shape: the increment inside an emulated geometry stage.
       Swept over the point count, because one wrong number does not say what
       the rule is -- an extra invocation, a doubled dispatch, a workgroup
       rounded up and a counter that is never reset all look alike at a single
       count. (It was the last of those.)

       Run twice, under the two ways an application may publish a write to a
       persistent mapping: MemoryBarrier, and -- the way the CTS does it on a
       4.5-or-better context -- GL_MAP_FLUSH_EXPLICIT_BIT with an explicit
       glFlushMappedBufferRange. They take different paths in the guest and
       only the first was covered. */
    GLuint gprogram = link2(vs, fs_plain, gs);
    eq("geometry_atomic_program_links", gprogram != 0, 1);
    if (gprogram) {
        glUseProgram(gprogram);
        for (unsigned mode = 0; mode < 2; ++mode) {
            const int explicit_flush = mode == 1;
            const char *how = explicit_flush ? "flush" : "barrier";
            if (explicit_flush) {
                /* Remap asking for the explicit-flush contract. */
                glUnmapBuffer(GL_ATOMIC_COUNTER_BUFFER);
                map = (GLuint *)glMapBufferRange(
                    GL_ATOMIC_COUNTER_BUFFER, 0, sizeof(GLuint),
                    GL_MAP_PERSISTENT_BIT | GL_MAP_READ_BIT |
                    GL_MAP_WRITE_BIT | GL_MAP_FLUSH_EXPLICIT_BIT);
                eq("explicit_flush_remap_succeeded", map != NULL, 1);
                if (!map) break;
            }
            static const unsigned points[] = { 1, 2, 3, 4, 8, 16 };
            for (unsigned k = 0; k < sizeof points / sizeof points[0]; ++k) {
                const unsigned want = points[k];
                *map = 0;
                if (explicit_flush)
                    glFlushMappedBufferRange(GL_ATOMIC_COUNTER_BUFFER, 0,
                                             sizeof(GLuint));
                else
                    glMemoryBarrier(GL_CLIENT_MAPPED_BUFFER_BARRIER_BIT);
                glClear(GL_COLOR_BUFFER_BIT);
                glDrawArrays(GL_POINTS, 0, (GLsizei)want);
                glMemoryBarrier(GL_ALL_BARRIER_BITS);
                GLuint direct = 0;
                glGetBufferSubData(GL_ATOMIC_COUNTER_BUFFER, 0, sizeof direct,
                                   &direct);
                char name[80];
                snprintf(name, sizeof name, "gs_%s_%02u_points_increment",
                         how, want);
                eq(name, direct, want);
                /* The mapping must agree with the plain read-back whatever
                   the count is: that is the persistent-mapping half. */
                snprintf(name, sizeof name, "gs_%s_%02u_map_matches", how,
                         want);
                eq(name, *map == direct, 1);
            }
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
