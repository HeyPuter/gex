/* Two atomic counter arrays at one binding, indexed by gl_PrimitiveIDIn from
 * an emulated geometry shader.
 *
 * What is left of KHR-GL44.buffer_storage.map_persistent_draw once the
 * persistent mapping is ruled out (gl44-persistent-atomic-probe, 31/31 on both
 * paths, both publishing routes). Its geometry shader declares
 *
 *     layout (binding = 0) uniform atomic_uint atom_color[2];
 *     layout (binding = 0) uniform atomic_uint invocation_hit_count[2];
 *
 * -- two arrays at the *same* binding, so four counters share one buffer at
 * offsets 0, 4, 8 and 12 -- and indexes both by gl_PrimitiveIDIn, guarding the
 * second increment on the first returning zero. Three things could go wrong
 * that a single counter cannot show: the offsets the two arrays get, the
 * primitive id the emulated stage reports, and whether the guard sees its own
 * increment.
 *
 * Two points, so each counter should end at exactly one. Each is checked
 * separately: which of the four is wrong says which of the three it was.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-38s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 440 core\n"
    "out uint vs_gs_index;\n"
    "void main() { vs_gs_index = uint(gl_VertexID);\n"
    "              gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
/* The CTS's own shader, less the uniform block it reads its rectangle from. */
static const char *gs_src =
    "#version 440 core\n"
    "layout(points)                           in;\n"
    "layout(triangle_strip, max_vertices = 4) out;\n"
    "layout (binding = 0) uniform atomic_uint atom_color[2];\n"
    "layout (binding = 0) uniform atomic_uint invocation_hit_count[2];\n"
    "in  uint  vs_gs_index[];\n"
    "out float gs_fs_color;\n"
    "void main() {\n"
    "    if (atomicCounterIncrement(invocation_hit_count[gl_PrimitiveIDIn]) == 0)\n"
    "    {\n"
    "        atomicCounterIncrement(atom_color[gl_PrimitiveIDIn]);\n"
    "    }\n"
    "    memoryBarrierAtomicCounter();\n"
    "    const uint atom_color_value = atomicCounter(atom_color[gl_PrimitiveIDIn]);\n"
    "    gs_fs_color = float(atom_color_value) / 255.0;\n"
    "    gl_Position = vec4(-1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4(-1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4( 1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4( 1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
    "}\n";
static const char *fs_src =
    "#version 440 core\n"
    "in  float gs_fs_color;\n"
    "out vec4  fs_out_color;\n"
    "void main() { fs_out_color = vec4(gs_fs_color, 0, 0, 1); }\n";

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2048] = {0};
               glGetShaderInfoLog(sh, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s compile failed: %s\n", what, log); }
    return ok ? sh : 0;
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
    GLuint gs = compile(GL_GEOMETRY_SHADER, gs_src, "gs");
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !gs || !fs) { printf("SUMMARY cases=0 passed=0 failed=0\n");
                            return 3; }
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, gs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[2048] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log); }
    eq("two_counter_arrays_link", linked, 1);
    if (!linked) { printf("SUMMARY cases=%d passed=%d failed=%d\n", cases,
                          passed, cases - passed); return 1; }

    /* Four counters in one buffer: atom_color[2] then invocation_hit_count[2],
       whichever order the linker assigns them. */
    GLuint atom = 0; glGenBuffers(1, &atom);
    const GLuint zeros[4] = { 0, 0, 0, 0 };
    glBindBuffer(GL_ATOMIC_COUNTER_BUFFER, atom);
    glBufferData(GL_ATOMIC_COUNTER_BUFFER, sizeof zeros, zeros,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_ATOMIC_COUNTER_BUFFER, 0, atom);
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glUseProgram(program);
    glViewport(0, 0, 16, 16);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_POINTS, 0, 2);
    glMemoryBarrier(GL_ALL_BARRIER_BITS);
    eq("counter_array_draw_is_clean", glGetError(), GL_NO_ERROR);

    GLuint got[4] = { 0, 0, 0, 0 };
    glGetBufferSubData(GL_ATOMIC_COUNTER_BUFFER, 0, sizeof got, got);
    fprintf(stderr, "INFO counters = %u %u %u %u\n", got[0], got[1], got[2],
            got[3]);
    /* Two points, one invocation each, so every one of the four counters ends
       at exactly one: each primitive increments its own hit count, sees zero,
       and increments its own colour. */
    eq("counter_0_is_1", got[0], 1);
    eq("counter_1_is_1", got[1], 1);
    eq("counter_2_is_1", got[2], 1);
    eq("counter_3_is_1", got[3], 1);
    /* And the colour the fragment stage saw, which is what the CTS checks in
       the framebuffer: atom_color is 1, so red is 1/255. */
    unsigned char pixel[4] = {0};
    glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    eq("fragment_saw_counter_value", pixel[0], 1);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
