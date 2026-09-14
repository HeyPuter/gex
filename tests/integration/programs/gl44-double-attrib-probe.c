/* Double-precision *vertex attributes* -- `glVertexAttribLPointer` and a
 * `dvec` input.
 *
 * Distinct from gl44-fp64-probe, which covers double *arithmetic* inside a
 * shader. This is about getting the bits in: WGSL has no 64-bit float, so a
 * `dvec4` input reaches Tint as an f64 and the whole module is refused --
 * "unhandled float width: 64" -- which arrives at the application as a **link
 * error**, so the program does not run at all.
 *
 * The fix has two halves and this probe measures both:
 *
 *   - the shader's input becomes `uvec4` halves, each double rebuilt with
 *     `pack_64_2x32_split`, which folds against the bitcasts
 *     `nir_lower_int64` has already left behind;
 *   - the *vertex element* splits, because st presents a double attribute as
 *     one element of `r32g32b32a32_uint` with `dual_slot` set -- "and another
 *     sixteen bytes at +16" -- so one GL attribute becomes two WebGPU ones at
 *     locations L and L+1, which GL has already reserved.
 *
 *   dvec2       four words, one location, one WebGPU attribute
 *   dvec3       six words: four and *two*. The upper half must be narrowed or
 *               the layout asks for sixteen bytes where the stride has eight,
 *               which WebGPU rejects.
 *   dvec4       eight words, the full pair
 *   captured    a dvec4 read in a shader whose result is recorded by
 *               transform feedback, which runs the vertex stage as *compute*
 *               and fetches the attribute out of the buffer itself rather
 *               than through vertex attributes at all
 *
 * The values are chosen so single precision cannot produce them: 1.0 + 2^-30
 * is exactly 1.0 in binary32 and distinguishable in binary64, so a driver
 * that quietly narrowed to float fails every case while still drawing.
 *
 * `glVertexAttribLPointer` is resolved through eglGetProcAddress rather than
 * linked, and that is not fastidiousness: this non-glvnd libGL exports
 * `glVertexAttribLFormat` and **none** of the other forty-five names in that
 * family, so linking the one this needs dies at load with "undefined symbol".
 * It is the standard Linux GL ABI -- libGL guarantees GL 1.2 and the rest
 * goes through getProcAddress, which is what GLEW, glad, GLFW and Qt all do --
 * but it is worth knowing when an application will not *start*. See the
 * handoff's section on the exported-symbol gap.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

#define SIDE 8

static int cases;
static int passed;

static void result(const char *name, int ok, const char *detail)
{
    ++cases;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-10s %s\n", name, detail);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* 1.0 + 2^-30: 1.0 exactly in binary32, distinguishable in binary64. */
#define FINE (1.0 + 1.0 / 1073741824.0)

/* The quad comes from gl_VertexID so the geometry is the same in every case
 * and only the attribute's *value* is under test. */
static const char *vs_head =
    "#version 400 core\n"
    "#extension GL_ARB_vertex_attrib_64bit : require\n";
static const char *vs_body =
    "out vec3 tint;\n"
    "void main() {\n"
    "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
    "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "  tint = vec3(0.0, ok() ? 1.0 : 0.0, 0.5);\n"
    "}\n";
static const char *fs_src =
    "#version 400 core\n"
    "in vec3 tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(tint, 1.0); }\n";

/* Each `ok()` compares against the double it was given, with a tolerance far
 * below what binary32 could represent.
 *
 * The fine value is *written out* as `1.0 + 1.0/1073741824.0` on both sides
 * rather than transcribed as a decimal. The first version of this probe put
 * `1.0009765625` in the shader -- which is 1 + 2^-10, not 1 + 2^-30 -- so
 * every case failed on the **reference**, which is the probe measuring its own
 * arithmetic. An expression both sides can share cannot drift. */
static const char *ok_dvec2 =
    "layout(location = 0) in dvec2 a;\n"
    "bool ok() {\n"
    "  return abs(a.x - (1.0lf + 1.0lf / 1073741824.0lf)) < 1.0e-12lf\n"
    "      && abs(a.y - 42.5lf) < 1.0e-12lf;\n"
    "}\n";
static const char *ok_dvec3 =
    "layout(location = 0) in dvec3 a;\n"
    "bool ok() {\n"
    "  return abs(a.x - (1.0lf + 1.0lf / 1073741824.0lf)) < 1.0e-12lf\n"
    "      && abs(a.y - 42.5lf) < 1.0e-12lf\n"
    "      && abs(a.z + 7.25lf) < 1.0e-12lf;\n"
    "}\n";
static const char *ok_dvec4 =
    "layout(location = 0) in dvec4 a;\n"
    "bool ok() {\n"
    "  return abs(a.x - (1.0lf + 1.0lf / 1073741824.0lf)) < 1.0e-12lf\n"
    "      && abs(a.y - 42.5lf) < 1.0e-12lf\n"
    "      && abs(a.z + 7.25lf) < 1.0e-12lf\n"
    "      && abs(a.w - (1.0lf + 1.0lf / 1073741824.0lf)) < 1.0e-12lf;\n"
    "}\n";

/* Two 64-bit attributes, and a matrix that *is* two of them.
 *
 * Every case above declares a single input at location 0, which cannot catch
 * a wrong location: there is nothing after it to collide with. A dvec4 takes
 * **two** attribute slots, so a second one belongs at location 2 -- and the
 * driver builds its @location numbers by walking a variable's slots and
 * advancing the vertex-element index in step, which is only the same walk
 * when every element is one slot wide.
 *
 * `dmat2x4` is the same shape arriving as one variable -- and its columns go
 * at locations 0 and **1**, not 0 and 2. The "a dvec4 consumes two locations"
 * rule separates distinct variables; a matrix's columns are consecutive. This
 * case bound its second column at 2 to begin with and drew nothing on
 * llvmpipe as well as here, which is what a case measuring its own setup
 * looks like: the second column was simply never supplied. The `a=`/`b=`/`m=`
 * numbers in each INFO line are printed so that assumption is visible rather
 * than implied. */
static const char *ok_two_dvec4 =
    "layout(location = 0) in dvec4 a;\n"
    "layout(location = 2) in dvec4 b;\n"
    "bool ok() {\n"
    "  return abs(a.x - (1.0lf + 1.0lf / 1073741824.0lf)) < 1.0e-12lf\n"
    "      && abs(a.w - (1.0lf + 1.0lf / 1073741824.0lf)) < 1.0e-12lf\n"
    "      && abs(b.x - 3.5lf) < 1.0e-12lf\n"
    "      && abs(b.y + 9.75lf) < 1.0e-12lf\n"
    "      && abs(b.w - 256.0lf) < 1.0e-12lf;\n"
    "}\n";
static const char *ok_dmat2x4 =
    "layout(location = 0) in dmat2x4 m;\n"
    "bool ok() {\n"
    "  return abs(m[0].x - (1.0lf + 1.0lf / 1073741824.0lf)) < 1.0e-12lf\n"
    "      && abs(m[0].w - (1.0lf + 1.0lf / 1073741824.0lf)) < 1.0e-12lf\n"
    "      && abs(m[1].x - 3.5lf) < 1.0e-12lf\n"
    "      && abs(m[1].y + 9.75lf) < 1.0e-12lf\n"
    "      && abs(m[1].w - 256.0lf) < 1.0e-12lf;\n"
    "}\n";

static GLuint build(const char *ok_src, const char *const *varying,
                    unsigned varyings)
{
    char vs[2048];
    snprintf(vs, sizeof(vs), "%s%s%s", vs_head, ok_src, vs_body);
    const char *vsp = vs;
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vsp, NULL); glCompileShader(v);
    GLint ok = 0; glGetShaderiv(v, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(v, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO vs compile: %s\n", log);
        return 0;
    }
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs_src, NULL); glCompileShader(f);
    GLuint p = glCreateProgram();
    glAttachShader(p, v); glAttachShader(p, f);
    if (varyings)
        glTransformFeedbackVaryings(p, (GLsizei)varyings, varying,
                                    GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(p);
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(p, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(p);
        return 0;
    }
    return p;
}

static int is_tint(const unsigned char *px)
{
    return px[0] < 32 && px[1] > 200 && px[2] > 96 && px[2] < 160;
}

static GLuint vbo;
static void (*attrib_lpointer)(GLuint, GLint, GLenum, GLsizei, const void *);

/* One case: upload `comps` doubles per vertex, draw, read the centre. */
static void run_case(const char *name, const char *ok_src, unsigned comps)
{
    /* Four vertices, all carrying the same values -- the test is the value,
     * not the per-vertex indexing, and every vertex reading it means the
     * whole quad is tinted or none of it is. */
    double data[4 * 4];
    const double want[4] = {FINE, 42.5, -7.25, FINE};
    for (unsigned v = 0; v < 4; ++v)
        for (unsigned c = 0; c < comps; ++c)
            data[v * comps + c] = want[c];
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER,
                 (GLsizeiptr)(4 * comps * sizeof(double)), data,
                 GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    attrib_lpointer(0, (GLint)comps, GL_DOUBLE,
                    (GLsizei)(comps * sizeof(double)), NULL);

    GLuint prog = build(ok_src, NULL, 0);
    const GLfloat clear[4] = {0.0f, 0.0f, 0.0f, 1.0f};
    glClearBufferfv(GL_COLOR, 0, clear);
    if (prog) { glUseProgram(prog); glDrawArrays(GL_TRIANGLE_STRIP, 0, 4); }
    glFinish();
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    char detail[160];
    snprintf(detail, sizeof(detail), "rgb=%u,%u,%u prog=%u err=0x%04x", px[0],
             px[1], px[2], prog, (unsigned)glGetError());
    result(name, prog && is_tint(px), detail);
    glDeleteProgram(prog);
    while (glGetError() != GL_NO_ERROR) { }
}

/* One case with two dvec4 attributes: eight doubles a vertex, bound at
 * locations 0 and 2. Both halves are checked, so binding the second at the
 * wrong location -- or reading the first one twice -- leaves the quad
 * untinted rather than coincidentally right. */
static void run_pair_case(const char *name, const char *ok_src,
                          GLuint second)
{
    double data[4 * 8];
    const double want[8] = {FINE, 42.5, -7.25, FINE, 3.5, -9.75, 0.125, 256.0};
    for (unsigned v = 0; v < 4; ++v)
        for (unsigned c = 0; c < 8; ++c)
            data[v * 8 + c] = want[c];
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)sizeof(data), data,
                 GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    attrib_lpointer(0, 4, GL_DOUBLE, (GLsizei)(8 * sizeof(double)), NULL);
    glEnableVertexAttribArray(second);
    attrib_lpointer(second, 4, GL_DOUBLE, (GLsizei)(8 * sizeof(double)),
                    (const void *)(4 * sizeof(double)));

    GLuint prog = build(ok_src, NULL, 0);
    const GLfloat clear[4] = {0.0f, 0.0f, 0.0f, 1.0f};
    glClearBufferfv(GL_COLOR, 0, clear);
    if (prog) { glUseProgram(prog); glDrawArrays(GL_TRIANGLE_STRIP, 0, 4); }
    glFinish();
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    /* Where the linker actually put the inputs.
     *
     * Both cases here bind locations 0 and 2, because a dvec4 consumes two
     * locations and so a second one -- or a matrix's second column -- belongs
     * at 2. `two_dvec4` passes everywhere, so that much is right for separate
     * variables. `dmat2x4` draws nothing on *both* this driver and llvmpipe,
     * which means the case is failing for a reason they share, and the first
     * thing to rule out is this assumption: if the columns are not at 0 and 2
     * then the case is measuring its own setup and not either driver. */
    char where[64] = "";
    if (prog) {
        const GLint at_a = glGetAttribLocation(prog, "a");
        const GLint at_b = glGetAttribLocation(prog, "b");
        const GLint at_m = glGetAttribLocation(prog, "m");
        snprintf(where, sizeof(where), " a=%d b=%d m=%d", at_a, at_b, at_m);
    }
    char detail[200];
    snprintf(detail, sizeof(detail), "rgb=%u,%u,%u prog=%u err=0x%04x%s", px[0],
             px[1], px[2], prog, (unsigned)glGetError(), where);
    result(name, prog && is_tint(px), detail);
    glDeleteProgram(prog);
    glDisableVertexAttribArray(second);
    while (glGetError() != GL_NO_ERROR) { }
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(d, NULL, NULL)) { printf("FATAL eglInitialize\n"); return 2; }
    static const EGLint ca[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE};
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    static const EGLint xa[] = {EGL_CONTEXT_MAJOR_VERSION, 4,
                                EGL_CONTEXT_MINOR_VERSION, 1, EGL_NONE};
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, xa);
    if (x == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 2; }
    static const EGLint sa[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE};
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!eglMakeCurrent(d, s, s, x)) { printf("FATAL eglMakeCurrent\n"); return 2; }
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    attrib_lpointer = (void (*)(GLuint, GLint, GLenum, GLsizei, const void *))
        eglGetProcAddress("glVertexAttribLPointer");
    if (!attrib_lpointer) {
        fprintf(stderr, "INFO no glVertexAttribLPointer entry point\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    GLuint vao = 0, tex = 0, fbo = 0;
    glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glGenBuffers(1, &vbo);
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, SIDE, SIDE);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n"); return 2;
    }
    glViewport(0, 0, SIDE, SIDE);
    glDisable(GL_DEPTH_TEST);

    run_case("dvec2", ok_dvec2, 2);
    run_case("dvec3", ok_dvec3, 3);
    run_case("dvec4", ok_dvec4, 4);
    run_pair_case("two_dvec4", ok_two_dvec4, 2);
    run_pair_case("dmat2x4", ok_dmat2x4, 1);

    /* 4. The same attribute, read by a shader whose output is recorded with
     *    transform feedback. That runs the vertex stage as a *compute*
     *    dispatch which fetches the attribute out of the vertex buffer
     *    itself, so it exercises a completely different reader from the three
     *    above -- and it is where lifting the link refusal first led to an
     *    aborted process, because the capture handed 32-bit words to a 64-bit
     *    load and nir_validate killed it. */
    {
        double data[4 * 4];
        const double want[4] = {FINE, 42.5, -7.25, FINE};
        for (unsigned v = 0; v < 4; ++v)
            for (unsigned k = 0; k < 4; ++k) data[v * 4 + k] = want[k];
        glBindBuffer(GL_ARRAY_BUFFER, vbo);
        glBufferData(GL_ARRAY_BUFFER, sizeof data, data, GL_STATIC_DRAW);
        glEnableVertexAttribArray(0);
        attrib_lpointer(0, 4, GL_DOUBLE, 4 * sizeof(double), NULL);

        static const char *varying[] = {"tint"};
        GLuint prog = build(ok_dvec4, varying, 1);
        GLuint xfb = 0;
        glGenBuffers(1, &xfb);
        glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, xfb);
        float seed[4 * 3];
        for (unsigned i = 0; i < 4 * 3; ++i) seed[i] = -1.0f;
        glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, sizeof seed, seed,
                     GL_DYNAMIC_READ);
        glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, xfb);
        float got[4 * 3];
        memset(got, 0, sizeof got);
        if (prog) {
            glUseProgram(prog);
            glBeginTransformFeedback(GL_TRIANGLES);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            glEndTransformFeedback();
            glFinish();
            glGetBufferSubData(GL_TRANSFORM_FEEDBACK_BUFFER, 0, sizeof got,
                               got);
        }
        char detail[160];
        snprintf(detail, sizeof(detail),
                 "v0=%.3f,%.3f,%.3f prog=%u err=0x%04x", (double)got[0],
                 (double)got[1], (double)got[2], prog, (unsigned)glGetError());
        /* The recorded tint is (0, 1, 0.5) only if the double arrived whole;
         * a capture that read zeros records (0, 0, 0.5), which is the shape
         * this is here to catch. */
        result("captured",
               prog && got[1] > 0.99f && got[1] < 1.01f, detail);
        glDeleteProgram(prog);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
