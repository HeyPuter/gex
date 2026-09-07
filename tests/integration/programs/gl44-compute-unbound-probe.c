/* A compute shader that declares resources the application never bound.
 *
 * GL leaves texture units enabled with nothing bound and says such a unit
 * samples as **opaque black** -- (0,0,0,1). Nothing about that is an error,
 * and a shader that declares a sampler it does not end up using is ordinary:
 * a uniform left at its default, a branch never taken, a debug path compiled
 * in. The graphics stages have always bound a 1x1 stand-in for it.
 *
 * The compute stage called gex_fail() instead, and gex_fail **latches the
 * context**: one dispatch declaring an unbound sampler killed every later
 * draw *and* dispatch in the program rather than its own. A latched context
 * reports no GL error, so the application saw its whole frame vanish with
 * nothing to explain it. Same for a unit whose sampler *state* was unset,
 * where the default-sampler helper was likewise sitting unused.
 *
 * The two fixes are one line each and neither moved a CTS score -- the case
 * that reaches them fails afterwards on an adapter limit -- so without this
 * probe a regression would be silent.
 *
 *   `unbound_texture_reads_zero`  a sampler2D with nothing bound reads
 *                                 (0,0,0,1), and the dispatch still runs
 *   `bound_neighbour_survives`    a *second* unit that IS bound reads its own
 *                                 texel, so the stand-in is not bound over it
 *   `later_dispatch_still_runs`   the real point: a dispatch after the one
 *                                 above still works. This is what latching
 *                                 broke, and it is invisible to a test that
 *                                 only checks the first dispatch's output.
 *
 * `later_dispatch_still_runs` is deliberately last and deliberately trivial.
 * If the context latched, it fails while the two before it may still pass.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLuint build_compute(const char *source) {
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        return 0;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return program;
}

/* Four floats out, so each case reads a named number rather than "it ran". */
static GLuint make_result_buffer(void) {
    GLuint buffer = 0;
    const float zeros[4] = {-1.0f, -1.0f, -1.0f, -1.0f};
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(zeros), zeros,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, buffer);
    return buffer;
}

static void read_result(GLuint buffer, float *out) {
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, 4 * sizeof(float), out);
}

static int near(float got, float want) {
    const float d = got - want;
    return (d < 0 ? -d : d) < 0.02f;
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    /* Unit 1 gets a real texture with one distinct texel; unit 0 gets
     * nothing at all. The shader samples both, so the driver has to bind a
     * stand-in for one and the real view for the other -- getting that
     * backwards reads 0.25 where zero belongs, or the reverse. */
    const unsigned char texel[4] = {64, 0, 0, 255};   /* 64/255 = 0.251 */
    GLuint bound = 0;
    glGenTextures(1, &bound);
    glActiveTexture(GL_TEXTURE1);
    glBindTexture(GL_TEXTURE_2D, bound);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, texel);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glActiveTexture(GL_TEXTURE0);

    static const char *unbound_source =
        "#version 430 core\n"
        "layout(local_size_x = 1) in;\n"
        "layout(binding = 0) uniform sampler2D u_absent;\n"
        "layout(binding = 1) uniform sampler2D u_present;\n"
        "layout(std430, binding = 0) buffer Out { float v[4]; } o;\n"
        "void main() {\n"
        "  vec4 a = texture(u_absent, vec2(0.5));\n"
        "  vec4 b = texture(u_present, vec2(0.5));\n"
        "  o.v[0] = a.r; o.v[1] = a.a; o.v[2] = b.r; o.v[3] = 1.0;\n"
        "}\n";

    GLuint result_buffer = make_result_buffer();
    GLuint program = build_compute(unbound_source);
    if (!program) {
        /* A link failure here is itself the defect: GL permits the shader. */
        result("unbound_texture_reads_zero", 0);
        result("bound_neighbour_survives", 0);
        result("later_dispatch_still_runs", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 0;
    }
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "u_absent"), 0);
    glUniform1i(glGetUniformLocation(program, "u_present"), 1);
    glDispatchCompute(1, 1, 1);

    float got[4] = {-1, -1, -1, -1};
    read_result(result_buffer, got);
    fprintf(stderr, "INFO absent=(%.3f,%.3f) present=%.3f ran=%.3f\n",
            got[0], got[1], got[2], got[3]);
    /* GL's opaque black: zero in rgb, one in alpha. */
    result("unbound_texture_reads_zero",
           near(got[0], 0.0f) && near(got[1], 1.0f) && near(got[3], 1.0f));
    result("bound_neighbour_survives", near(got[2], 64.0f / 255.0f));

    /* And now the part latching broke. Nothing exotic -- a dispatch that
     * writes a constant -- so the only way it fails is if the context was
     * failed by the dispatch above. */
    static const char *plain_source =
        "#version 430 core\n"
        "layout(local_size_x = 1) in;\n"
        "layout(std430, binding = 0) buffer Out { float v[4]; } o;\n"
        "void main() { o.v[0] = 0.75; o.v[1] = 0.75;"
        " o.v[2] = 0.75; o.v[3] = 0.75; }\n";
    GLuint plain = build_compute(plain_source);
    int later_ok = 0;
    if (plain) {
        glUseProgram(plain);
        glDispatchCompute(1, 1, 1);
        float after[4] = {-1, -1, -1, -1};
        read_result(result_buffer, after);
        fprintf(stderr, "INFO later=%.3f\n", after[0]);
        later_ok = near(after[0], 0.75f);
    }
    result("later_dispatch_still_runs", later_ok);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
