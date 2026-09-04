/* Write-only shader images in formats WebGPU has no storage texture for.
 *
 * WebGPU offers storage textures for the four-channel 8/16/32-bit formats and
 * the one- and two-channel 32-bit ones, and that is all. GL 4.2 requires
 * eighteen more: r11f_g11f_b10f, r16f, rg16f, r8ui, rgb10_a2 and every 16-bit
 * normalised format among them. This driver used to *refuse* such a shader,
 * and a refusal at link time is worse than a wrong pixel -- Blender's EEVEE
 * writes r11f_g11f_b10f and r16f images, got a null program back and dumped
 * core on it.
 *
 * They are served through a plain storage buffer now: the shader packs the
 * texel itself and the driver copies the buffer into the texture when the
 * pass ends. So this probe is about whether the packing is *right*, which a
 * link-success test would not notice.
 *
 *   `r11f_g11f_b10f`  three channels at 11/11/10 bits, the format EEVEE's
 *                     radiance and scattering images use
 *   `r16f`            two bytes per texel, so several texels share a 32-bit
 *                     word of the buffer and neighbouring invocations write
 *                     different halves of it -- the case a read-modify-write
 *                     loses and two atomics keep
 *   `rg16f`           four bytes, two channels: catches a packer that wrote
 *                     the channels in the wrong order or the wrong width
 *   `r8ui`            one byte, and an integer format, so four texels share a
 *                     word and no float conversion may happen
 *   `stride`          the far corner of a wide image, which is what a row
 *                     stride computed from the width rather than from the
 *                     copy's 256-byte alignment gets wrong
 *   `image_size`      imageSize() reports the texture's dimensions and not
 *                     the padded row the buffer really has
 *
 * Every value stored depends on the texel's coordinate, so a driver that
 * wrote one texel everywhere, or transposed x and y, fails rather than
 * passing on a uniform colour. The read back is a second draw sampling the
 * texture, not a map: that is the path an application uses, and it is what
 * proves the copy out of the buffer happened at all.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <math.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* Wide enough that width * bytes-per-texel is not a multiple of 256 for any
 * of the formats here, so a stride bug cannot hide. */
#define WIDTH 40
#define HEIGHT 6

static GLuint compile(GLenum stage, const char *source, const char *label) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s compile: %s\n", label, log);
        return 0;
    }
    return shader;
}

static GLuint link(const char *vs_src, const char *fs_src, const char *label) {
    GLuint vs = compile(GL_VERTEX_SHADER, vs_src, label);
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, label);
    if (!vs || !fs) return 0;
    GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    GLint ok = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s link: %s\n", label, log);
        return 0;
    }
    return program;
}

static const char *vs_quad =
    "#version 420 core\n"
    "void main() {\n"
    "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
    "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "}\n";

struct format {
    const char *name;
    GLenum internal;            /* the texture's sized internal format */
    const char *qualifier;      /* the GLSL image format layout qualifier */
    const char *image_type;     /* image2D or uimage2D */
    const char *store;          /* the value stored, in terms of `at` */
    const char *sampler;        /* sampler2D or usampler2D */
    /* What sampling texel (x,y) must give back, as a float in 0..255 after
     * the read-back shader has scaled it. */
    int channels;
};

/* The value written at texel (x,y), and what the read-back must show. Kept in
 * C as well as in GLSL so the two cannot drift apart silently: the shader
 * computes it from gl_FragCoord and this computes it from the coordinate. */
static void expected(const struct format *f, int x, int y, float *out) {
    const float u = (float)x / (float)WIDTH;
    const float v = (float)y / (float)HEIGHT;
    out[0] = u; out[1] = v; out[2] = 0.5f; out[3] = 1.0f;
    if (strcmp(f->name, "r8ui") == 0) {
        out[0] = (float)((x * 5 + y) & 0xff);
        out[1] = out[2] = out[3] = 0.0f;
    }
}

static const struct format formats[] = {
    {"r11f_g11f_b10f", GL_R11F_G11F_B10F, "r11f_g11f_b10f", "image2D",
     "vec4(u, v, 0.5, 1.0)", "sampler2D", 3},
    {"r16f", GL_R16F, "r16f", "image2D",
     "vec4(u, v, 0.5, 1.0)", "sampler2D", 1},
    {"rg16f", GL_RG16F, "rg16f", "image2D",
     "vec4(u, v, 0.5, 1.0)", "sampler2D", 2},
    {"r8ui", GL_R8UI, "r8ui", "uimage2D",
     "uvec4(uint((x * 5 + y) & 0xff), 0u, 0u, 0u)", "usampler2D", 1},
};

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

    GLint major = 0, minor = 0;
    glGetIntegerv(GL_MAJOR_VERSION, &major);
    glGetIntegerv(GL_MINOR_VERSION, &minor);
    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));
    if (major * 10 + minor < 42) {
        fprintf(stderr, "INFO image load/store needs GL 4.2\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    /* The read-back target: ordinary rgba8, so readPixels is trivial. */
    GLuint target = 0, fbo = 0, vao = 0;
    glGenTextures(1, &target);
    glBindTexture(GL_TEXTURE_2D, target);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, WIDTH, HEIGHT, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           target, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, WIDTH, HEIGHT);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* The two sample points: one near the origin and one in the far corner,
     * whose row is the one a mis-computed stride misses. */
    const int probe_x[2] = {1, WIDTH - 2};
    const int probe_y[2] = {0, HEIGHT - 1};

    for (unsigned f = 0; f < sizeof(formats) / sizeof(formats[0]); ++f) {
        const struct format *fm = &formats[f];
        GLuint tex = 0;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, fm->internal, WIDTH, HEIGHT);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        GLenum error = glGetError();
        if (error != GL_NO_ERROR) {
            fprintf(stderr, "INFO %s: texture storage err=0x%04x\n", fm->name,
                    (unsigned)error);
            result(fm->name, 0);
            continue;
        }

        char store_src[900];
        snprintf(store_src, sizeof(store_src),
                 "#version 420 core\n"
                 "layout(%s) uniform writeonly %s dst;\n"
                 "out vec4 colour;\n"
                 "void main() {\n"
                 "  int x = int(gl_FragCoord.x);\n"
                 "  int y = int(gl_FragCoord.y);\n"
                 "  float u = float(x) / %d.0;\n"
                 "  float v = float(y) / %d.0;\n"
                 "  imageStore(dst, ivec2(x, y), %s);\n"
                 "  colour = vec4(0.0, 0.0, 0.0, 1.0);\n"
                 "}\n",
                 fm->qualifier, fm->image_type, WIDTH, HEIGHT, fm->store);
        GLuint store_program = link(vs_quad, store_src, fm->name);
        if (!store_program) { result(fm->name, 0); continue; }

        glUseProgram(store_program);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_WRITE_ONLY,
                           fm->internal);
        glUniform1i(glGetUniformLocation(store_program, "dst"), 0);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                        GL_TEXTURE_UPDATE_BARRIER_BIT);
        error = glGetError();
        if (error != GL_NO_ERROR) {
            fprintf(stderr, "INFO %s: store draw err=0x%04x\n", fm->name,
                    (unsigned)error);
            result(fm->name, 0);
            continue;
        }

        /* Read it back by sampling. The scale puts each format's value into
         * 0..255 so one rgba8 target serves all of them: the integer format
         * carries its value directly, the float ones are 0..1. */
        char fetch_src[900];
        snprintf(fetch_src, sizeof(fetch_src),
                 "#version 420 core\n"
                 "uniform %s src;\n"
                 "out vec4 colour;\n"
                 "void main() {\n"
                 "  ivec2 at = ivec2(gl_FragCoord.xy);\n"
                 "  %s v = texelFetch(src, at, 0);\n"
                 "  colour = %s;\n"
                 "}\n",
                 fm->sampler,
                 strcmp(fm->sampler, "usampler2D") == 0 ? "uvec4" : "vec4",
                 strcmp(fm->sampler, "usampler2D") == 0
                    ? "vec4(vec3(v.xyz) / 255.0, 1.0)"
                    : "vec4(v.xyz, 1.0)");
        GLuint fetch_program = link(vs_quad, fetch_src, fm->name);
        if (!fetch_program) { result(fm->name, 0); continue; }
        glUseProgram(fetch_program);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, tex);
        glUniform1i(glGetUniformLocation(fetch_program, "src"), 0);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

        int ok = 1;
        for (unsigned p = 0; p < 2; ++p) {
            unsigned char got[4] = {0};
            glReadPixels(probe_x[p], probe_y[p], 1, 1, GL_RGBA,
                         GL_UNSIGNED_BYTE, got);
            float want[4];
            expected(fm, probe_x[p], probe_y[p], want);
            for (int c = 0; c < fm->channels; ++c) {
                const float scale =
                    strcmp(fm->name, "r8ui") == 0 ? 1.0f : 255.0f;
                const float target_value = want[c] * scale;
                /* Two counts of slack: 11-bit and 10-bit floats are coarse,
                 * and the read back passes through an 8-bit target. */
                if (fabsf((float)got[c] - target_value) > 3.0f)
                    ok = 0;
            }
            fprintf(stderr, "INFO %s at %d,%d got=%u,%u,%u want=%.1f,%.1f,"
                    "%.1f (%d channels)\n", fm->name, probe_x[p], probe_y[p],
                    got[0], got[1], got[2],
                    want[0] * (strcmp(fm->name, "r8ui") == 0 ? 1.0f : 255.0f),
                    want[1] * (strcmp(fm->name, "r8ui") == 0 ? 1.0f : 255.0f),
                    want[2] * (strcmp(fm->name, "r8ui") == 0 ? 1.0f : 255.0f),
                    fm->channels);
        }
        error = glGetError();
        result(fm->name, ok && error == GL_NO_ERROR);
        /* The far-corner point above is the stride case; report it once, on
         * the format most likely to expose it. */
        if (strcmp(fm->name, "r11f_g11f_b10f") == 0)
            result("stride", ok && error == GL_NO_ERROR);
    }

    /* A fragment image read *and* written. WebGPU has storage textures for
     * rgba16f but will not give them read_write access -- only r32ui/r32i/
     * r32f get that -- so this is packed for a different reason from the
     * cases above, and it is the one that needs the texture copied *into* the
     * buffer before the draw. Seeding it is what this checks: the shader adds
     * to what is already there, so a driver that bound an unseeded buffer
     * reads zero and the sum comes out short. */
    {
        GLuint tex = 0;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA16F, WIDTH, HEIGHT);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        /* 0.25 everywhere, so the shader's +0.5 must read back as 0.75. */
        static float seed[WIDTH * HEIGHT * 4];
        for (unsigned i = 0; i < WIDTH * HEIGHT * 4; ++i) seed[i] = 0.25f;
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, WIDTH, HEIGHT, GL_RGBA,
                        GL_FLOAT, seed);
        static const char *rmw_src =
            "#version 420 core\n"
            "layout(rgba16f) uniform image2D img;\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  ivec2 at = ivec2(gl_FragCoord.xy);\n"
            "  vec4 was = imageLoad(img, at);\n"
            "  imageStore(img, at, was + vec4(0.5));\n"
            "  colour = vec4(0.0, 0.0, 0.0, 1.0);\n"
            "}\n";
        GLuint program = link(vs_quad, rmw_src, "read_write");
        int ok = 0;
        if (program) {
            glUseProgram(program);
            glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_WRITE,
                               GL_RGBA16F);
            glUniform1i(glGetUniformLocation(program, "img"), 0);
            glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                            GL_TEXTURE_UPDATE_BARRIER_BIT);
            /* Read back by sampling, as above. */
            GLuint fetch = link(vs_quad,
                "#version 420 core\n"
                "uniform sampler2D src;\n"
                "out vec4 colour;\n"
                "void main() {\n"
                "  colour = vec4(texelFetch(src, ivec2(gl_FragCoord.xy),\n"
                "                           0).xyz, 1.0);\n"
                "}\n", "read_write");
            if (fetch) {
                glUseProgram(fetch);
                glActiveTexture(GL_TEXTURE0);
                glBindTexture(GL_TEXTURE_2D, tex);
                glUniform1i(glGetUniformLocation(fetch, "src"), 0);
                glClear(GL_COLOR_BUFFER_BIT);
                glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
                unsigned char got[4] = {0};
                glReadPixels(WIDTH - 2, HEIGHT - 1, 1, 1, GL_RGBA,
                             GL_UNSIGNED_BYTE, got);
                const GLenum error = glGetError();
                fprintf(stderr, "INFO read_write got=%u,%u,%u want~=191 "
                        "err=0x%04x\n", got[0], got[1], got[2],
                        (unsigned)error);
                /* 0.75 of 255 is 191; an unseeded buffer gives 0.5 -> 128. */
                ok = error == GL_NO_ERROR && got[0] >= 187 && got[0] <= 195 &&
                     got[1] >= 187 && got[1] <= 195;
            }
        }
        result("read_write", ok);
    }

    /* imageSize must report the texture, not the padded buffer row. */
    {
        GLuint tex = 0;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_R11F_G11F_B10F, WIDTH, HEIGHT);
        char size_src[700];
        snprintf(size_src, sizeof(size_src),
                 /* imageSize() is GLSL 4.30 (ARB_shader_image_size), not
                  * 4.20 -- llvmpipe rejects it at 4.20 too, which is how the
                  * probe's own bug showed up rather than the driver's. */
                 "#version 430 core\n"
                 "layout(r11f_g11f_b10f) uniform writeonly image2D dst;\n"
                 "out vec4 colour;\n"
                 "void main() {\n"
                 "  ivec2 s = imageSize(dst);\n"
                 "  imageStore(dst, ivec2(gl_FragCoord.xy),\n"
                 "             vec4(0.0, 0.0, 0.0, 1.0));\n"
                 "  colour = vec4(s.x == %d ? 1.0 : 0.0,\n"
                 "                s.y == %d ? 1.0 : 0.0, 0.0, 1.0);\n"
                 "}\n", WIDTH, HEIGHT);
        GLuint program = link(vs_quad, size_src, "image_size");
        int ok = 0;
        if (program) {
            glUseProgram(program);
            glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_WRITE_ONLY,
                               GL_R11F_G11F_B10F);
            glUniform1i(glGetUniformLocation(program, "dst"), 0);
            glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            unsigned char got[4] = {0};
            glReadPixels(WIDTH / 2, HEIGHT / 2, 1, 1, GL_RGBA,
                         GL_UNSIGNED_BYTE, got);
            const GLenum error = glGetError();
            fprintf(stderr, "INFO image_size got=%u,%u err=0x%04x\n", got[0],
                    got[1], (unsigned)error);
            ok = error == GL_NO_ERROR && got[0] > 128 && got[1] > 128;
        }
        result("image_size", ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
