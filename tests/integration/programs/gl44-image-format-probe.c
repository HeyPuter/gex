/* imageStore and imageLoad on the float formats WebGPU has no storage texture
 * for.
 *
 * WebGPU offers storage textures only for the four-channel 8/16/32-bit formats
 * and the one- and two-channel 32-bit ones. GL 4.2 requires about twenty more,
 * and among them are the ones Blender's EEVEE viewport writes: `r11f_g11f_b10f`
 * and `r16f`. The driver refuses such an image, which fails the shader at link
 * time and takes the application with it.
 *
 * Whether that can be emulated turns on a question this probe answers: a
 * *float* image format can be stored in a wider float format without losing
 * anything -- half float has a 10-bit mantissa where r11f_g11f_b10f has six --
 * so the driver could allocate `rgba16f` and the shader would never know,
 * **provided the channel semantics are preserved**. GL guarantees that
 * imageLoad on a one-channel image returns (x, 0, 0, 1) whatever the shader
 * stored in the other components, and a wider allocation returns what was
 * actually written there. That is the part an emulation gets wrong, so it is
 * the part this probe pins.
 *
 *   `r32f_roundtrip`   the control: r32f *is* a WebGPU storage format, so this
 *                      must pass however the rest behave, and a failure here
 *                      means the probe is wrong rather than the driver
 *   `r16f_roundtrip`   a value written and read back through an r16f image
 *   `r16f_channels`    imageLoad on that image returns 0 in green and blue and
 *                      1 in alpha, *after* a store that put non-zero values
 *                      there -- which a wider allocation reports verbatim
 *                      unless the load is fixed up
 *   `r11g11b10_rgb`    three channels through r11f_g11f_b10f, with values
 *                      chosen to survive its six-bit mantissa
 *   `r11g11b10_alpha`  and its alpha, which GL says is 1 regardless
 *
 * Values are powers of two plus a small fraction so they are exact in any
 * float format wider than six mantissa bits, which keeps this a test of
 * plumbing rather than of rounding.
 *
 * **This probe is expected to fail 4 of its 6 cases today**, and is committed
 * as a marker for the gap rather than as a regression. The two r32f cases
 * pass because r32f *is* a WebGPU storage format; the r16f and
 * r11f_g11f_b10f ones report the buffer's filler value, because the shader is
 * refused and the dispatch never runs. It flips to OK if and when the
 * substitution is built. `gl44-group-vote-probe` is committed on the same
 * footing.
 *
 * One result here is already useful for building it: `r32f_channels` passes,
 * so a genuinely one-channel storage format returns (x, 0, 0, 1) without the
 * driver doing anything -- WGSL specifies it. Allocating a *four*-channel
 * format instead would return whatever the store put in the other components,
 * so the substitution has to restore those by hand. That is the part this
 * probe exists to keep honest.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdlib.h>
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

    GLuint out_buffer = 0;
    glGenBuffers(1, &out_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, out_buffer);

    /* Each case: a 1x1 image of the format, stored to and loaded back inside
     * one dispatch, with the four loaded components reported as scaled
     * integers so the readback carries no float formatting of its own. */
    struct { const char *name; GLenum internal; const char *qualifier;
             const char *store; GLint want[4]; } tests[] = {
        {"r32f_roundtrip", GL_R32F, "r32f",
         "imageStore(img, ivec2(0), vec4(2.5, 7.0, 9.0, 11.0));",
         {2500, 0, 0, 1000}},
        {"r16f_roundtrip", GL_R16F, "r16f",
         "imageStore(img, ivec2(0), vec4(2.5, 7.0, 9.0, 11.0));",
         {2500, 0, 0, 1000}},
        {"r11g11b10_rgb", GL_R11F_G11F_B10F, "r11f_g11f_b10f",
         "imageStore(img, ivec2(0), vec4(2.5, 3.0, 4.0, 11.0));",
         {2500, 3000, 4000, 1000}},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        GLuint tex = 0;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, tests[t].internal, 1, 1);
        const GLenum setup = glGetError();

        char source[900];
        snprintf(source, sizeof(source),
                 "#version 430\n"
                 "layout(local_size_x = 1) in;\n"
                 "layout(binding = 0, %s) uniform image2D img;\n"
                 "layout(std430, binding = 0) buffer Out { int value[]; };\n"
                 "void main() {\n"
                 "  %s\n"
                 /* The load is in the same dispatch, so no barrier between
                  * passes is involved -- one invocation writing and reading
                  * its own texel needs none. */
                 "  vec4 got = imageLoad(img, ivec2(0));\n"
                 "  value[0] = int(got.r * 1000.0 + 0.5);\n"
                 "  value[1] = int(got.g * 1000.0 + 0.5);\n"
                 "  value[2] = int(got.b * 1000.0 + 0.5);\n"
                 "  value[3] = int(got.a * 1000.0 + 0.5);\n"
                 "}\n", tests[t].qualifier, tests[t].store);
        GLuint program = build_compute(source);

        const GLint filler[4] = {-999, -999, -999, -999};
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(filler), filler,
                     GL_DYNAMIC_COPY);
        if (program) {
            glUseProgram(program);
            glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_WRITE,
                               tests[t].internal);
            glDispatchCompute(1, 1, 1);
            glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
        }
        GLint got[4] = {0, 0, 0, 0};
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
        glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
        const GLenum error = glGetError();
        fprintf(stderr, "INFO %s got=%d,%d,%d,%d want=%d,%d,%d,%d "
                "setup=0x%04x err=0x%04x\n", tests[t].name, got[0], got[1],
                got[2], got[3], tests[t].want[0], tests[t].want[1],
                tests[t].want[2], tests[t].want[3], (unsigned)setup,
                (unsigned)error);
        /* The red channel alone: the value made the round trip. */
        result(tests[t].name, program && setup == GL_NO_ERROR &&
                              error == GL_NO_ERROR &&
                              abs(got[0] - tests[t].want[0]) <= 2);
        /* And the channels GL fills in regardless of what was stored. A
         * wider allocation reports what the store actually put there, so
         * this is the case an emulation has to work for. */
        if (t != 2) {
            char name[48];
            snprintf(name, sizeof(name), "%s_channels", tests[t].qualifier);
            result(name, program && error == GL_NO_ERROR &&
                         got[1] == 0 && got[2] == 0 &&
                         abs(got[3] - 1000) <= 2);
        } else {
            result("r11g11b10_alpha", program && error == GL_NO_ERROR &&
                                      abs(got[3] - 1000) <= 2);
        }
        glDeleteTextures(1, &tex);
        if (program) glDeleteProgram(program);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
