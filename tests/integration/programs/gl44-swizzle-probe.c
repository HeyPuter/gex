/* ARB_texture_swizzle across format classes.
 *
 * KHR-GL33.texture_swizzle's functional matrix fails on every fixed-point and
 * integer format and passes only on the floating-point ones -- identically for
 * every texture target, so the variable is the format, not the target. This
 * reduces that to one texture per format class, one swizzle each, checked
 * against a real driver.
 *
 * The swizzles are chosen so a driver that ignores them, or applies them to
 * the wrong channel, cannot accidentally produce the right answer: each case
 * moves a distinct non-zero value into a channel that would otherwise hold
 * something else.
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

static const char *vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

static const char *fragment_source =
    "#version 330 core\n"
    "uniform sampler2D tex;\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = texture(tex, vec2(0.5, 0.5)); }\n";

/* The access routine the CTS functional matrix uses, and the one the other
 * cases here do not: texelFetch takes a different path through the driver --
 * gex_wgpu_lower_view_swizzle_instr -- than texture(), which ordinary 2D
 * float sampling hands to the nearest-border pass instead. */
static const char *fetch_fragment_source =
    "#version 330 core\n"
    "uniform sampler2D tex;\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = texelFetch(tex, ivec2(0, 0), 0); }\n";

static GLuint compile(GLenum stage, const char *source) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "shader compile failed: %s\n", log);
        return 0;
    }
    return shader;
}

struct swizzle_case {
    const char *name;
    GLenum internal_format;
    GLenum format;
    GLenum type;
    /* One texel, as the bytes the upload takes. */
    unsigned char bytes[8];
    unsigned byte_count;
    GLint swizzle[4];
    unsigned char expected[4];
};

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) return 2;
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "link failed: %s\n", log);
        return 2;
    }
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "tex"), 0);

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 4, 4, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 4, 4);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    static const float positions[12] = {
        -1.0f, -1.0f,  1.0f, -1.0f,  -1.0f, 1.0f,
         1.0f, -1.0f,  1.0f,  1.0f,  -1.0f, 1.0f,
    };
    GLuint position_buffer = 0;
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(positions), positions, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    glActiveTexture(GL_TEXTURE0);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);

    const struct swizzle_case tests[] = {
        /* Identity first, so a failure here separates "swizzle is wrong" from
         * "sampling this format is wrong". */
        {"rgba8_identity", GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE,
         {40, 80, 120, 255}, 4,
         {GL_RED, GL_GREEN, GL_BLUE, GL_ALPHA}, {40, 80, 120, 255}},
        /* Reverse: every channel moves, so a dropped swizzle is obvious. */
        {"rgba8_reversed", GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE,
         {40, 80, 120, 200}, 4,
         {GL_ALPHA, GL_BLUE, GL_GREEN, GL_RED}, {200, 120, 80, 40}},
        /* ZERO and ONE are the two constants the swizzle can name. */
        {"rgba8_zero_one", GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE,
         {40, 80, 120, 200}, 4,
         {GL_ZERO, GL_ONE, GL_RED, GL_GREEN}, {0, 255, 40, 80}},
        /* One channel broadcast: R8's green and blue read zero by default, so
         * this is the case that shows whether the source channel is right. */
        {"r8_broadcast", GL_R8, GL_RED, GL_UNSIGNED_BYTE,
         {150}, 1,
         {GL_RED, GL_RED, GL_RED, GL_ONE}, {150, 150, 150, 255}},
        {"rg8_swapped", GL_RG8, GL_RG, GL_UNSIGNED_BYTE,
         {60, 180}, 2,
         {GL_GREEN, GL_RED, GL_ZERO, GL_ONE}, {180, 60, 0, 255}},
        /* The float class, which is the one the CTS matrix already passes --
         * kept so a regression there is visible in the same run. */
        {"r16f_broadcast", GL_R16F, GL_RED, GL_HALF_FLOAT,
         {0x00, 0x38}, 2,   /* half 0.5 */
         {GL_RED, GL_RED, GL_RED, GL_ONE}, {128, 128, 128, 255}},
    };

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        const struct swizzle_case *test = &tests[i];
        GLuint texture = 0;
        glGenTextures(1, &texture);
        glBindTexture(GL_TEXTURE_2D, texture);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
        glTexImage2D(GL_TEXTURE_2D, 0, test->internal_format, 1, 1, 0,
                     test->format, test->type, test->bytes);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_R, test->swizzle[0]);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_G, test->swizzle[1]);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_B, test->swizzle[2]);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_A, test->swizzle[3]);
        const GLenum setup_error = glGetError();

        glClearColor(0.0f, 0.0f, 0.0f, 0.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 6);

        unsigned char got[4] = {0xAB, 0xAB, 0xAB, 0xAB};
        glReadPixels(2, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
        const GLenum error = glGetError();

        /* Two counts of slack: half-float 0.5 and any normalization land on
         * boundaries whose rounding is the GPU's business. */
        int ok = setup_error == GL_NO_ERROR && error == GL_NO_ERROR;
        for (unsigned c = 0; c < 4; ++c) {
            const int delta = (int)got[c] - (int)test->expected[c];
            if (delta > 2 || delta < -2) ok = 0;
        }
        fprintf(stderr, "INFO %s got=%u,%u,%u,%u expected=%u,%u,%u,%u "
                "setup=0x%04x err=0x%04x\n", test->name,
                got[0], got[1], got[2], got[3], test->expected[0],
                test->expected[1], test->expected[2], test->expected[3],
                (unsigned)setup_error, (unsigned)error);
        result(test->name, ok);
        glDeleteTextures(1, &texture);
    }

    /* glGetTexImage: read a texture's contents straight back, no rendering.
     *
     * KHR-GL33.texture_swizzle's functional matrix reads its output with
     * glGetTexImage (gl3cTextureSwizzleTests.cpp:2622) rather than
     * glReadPixels, and that matrix fails on every fixed-point and integer
     * format while passing on the floating-point ones. Sampling those same
     * formats is correct -- the cases above prove it -- so the readback is
     * the part left untested. */
    {
        static const struct {
            const char *name;
            GLenum internal_format, format, type;
            unsigned char bytes[4];
            unsigned byte_count;
            unsigned char expected[4];
        } reads[] = {
            {"getteximage_rgba8", GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE,
             {40, 80, 120, 200}, 4, {40, 80, 120, 200}},
            {"getteximage_r8", GL_R8, GL_RED, GL_UNSIGNED_BYTE,
             {150}, 1, {150, 0, 0, 0}},
        };
        for (unsigned i = 0; i < sizeof(reads) / sizeof(reads[0]); ++i) {
            GLuint texture = 0;
            glGenTextures(1, &texture);
            glBindTexture(GL_TEXTURE_2D, texture);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
            glTexImage2D(GL_TEXTURE_2D, 0, reads[i].internal_format, 1, 1, 0,
                         reads[i].format, reads[i].type, reads[i].bytes);
            unsigned char back[8];
            memset(back, 0xAB, sizeof(back));
            glGetTexImage(GL_TEXTURE_2D, 0, reads[i].format, reads[i].type,
                          back);
            const GLenum error = glGetError();
            int ok = error == GL_NO_ERROR;
            for (unsigned c = 0; c < reads[i].byte_count; ++c)
                if (back[c] != reads[i].expected[c]) ok = 0;
            fprintf(stderr, "INFO %s got=%u,%u,%u,%u expected=%u,%u,%u,%u "
                    "err=0x%04x\n", reads[i].name, back[0], back[1], back[2],
                    back[3], reads[i].expected[0], reads[i].expected[1],
                    reads[i].expected[2], reads[i].expected[3],
                    (unsigned)error);
            result(reads[i].name, ok);
            glDeleteTextures(1, &texture);
        }
    }

    /* Render into a texture of each format class, then read it back with
     * glGetTexImage -- which is what the CTS functional matrix does: its
     * output texture matches the *source* format's class, so an R8 source is
     * checked by rendering into an R8 target. Everything above rendered to
     * RGBA8, so this is the part still untested, and the pass/fail split in
     * the matrix is by format class. */
    {
        static const struct {
            const char *name;
            GLenum internal_format, format, type;
            unsigned char expected[4];
            unsigned check;
        } targets[] = {
            {"rendertarget_rgba8", GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE,
             {51, 102, 153, 204}, 4},
            {"rendertarget_r8", GL_R8, GL_RED, GL_UNSIGNED_BYTE,
             {51, 0, 0, 0}, 1},
            {"rendertarget_rg8", GL_RG8, GL_RG, GL_UNSIGNED_BYTE,
             {51, 102, 0, 0}, 2},
        };
        GLuint constant_fs = compile(GL_FRAGMENT_SHADER,
            "#version 330 core\n"
            "out vec4 fragColour;\n"
            "void main() { fragColour = vec4(0.2, 0.4, 0.6, 0.8); }\n");
        GLuint constant_prog = glCreateProgram();
        glAttachShader(constant_prog, vertex);
        glAttachShader(constant_prog, constant_fs);
        glLinkProgram(constant_prog);
        glUseProgram(constant_prog);
        for (unsigned i = 0; i < sizeof(targets) / sizeof(targets[0]); ++i) {
            GLuint out_tex = 0, out_fb = 0;
            glGenTextures(1, &out_tex);
            glBindTexture(GL_TEXTURE_2D, out_tex);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
            glTexImage2D(GL_TEXTURE_2D, 0, targets[i].internal_format, 1, 1, 0,
                         targets[i].format, targets[i].type, NULL);
            glGenFramebuffers(1, &out_fb);
            glBindFramebuffer(GL_FRAMEBUFFER, out_fb);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                   GL_TEXTURE_2D, out_tex, 0);
            const GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            glViewport(0, 0, 1, 1);
            glClearColor(0.0f, 0.0f, 0.0f, 0.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLES, 0, 6);
            unsigned char back[8];
            memset(back, 0xAB, sizeof(back));
            glBindTexture(GL_TEXTURE_2D, out_tex);
            glGetTexImage(GL_TEXTURE_2D, 0, targets[i].format, targets[i].type,
                          back);
            const GLenum error = glGetError();
            int ok = error == GL_NO_ERROR && status == GL_FRAMEBUFFER_COMPLETE;
            for (unsigned c = 0; c < targets[i].check; ++c) {
                const int delta = (int)back[c] - (int)targets[i].expected[c];
                if (delta > 2 || delta < -2) ok = 0;
            }
            fprintf(stderr, "INFO %s status=0x%04x got=%u,%u,%u,%u "
                    "expected=%u,%u,%u,%u err=0x%04x\n", targets[i].name,
                    (unsigned)status, back[0], back[1], back[2], back[3],
                    targets[i].expected[0], targets[i].expected[1],
                    targets[i].expected[2], targets[i].expected[3],
                    (unsigned)error);
            result(targets[i].name, ok);
            glDeleteFramebuffers(1, &out_fb);
            glDeleteTextures(1, &out_tex);
        }
        glUseProgram(program);
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glViewport(0, 0, 4, 4);
    }

    /* Same swizzles again, fetched rather than sampled. */
    {
        GLuint fetch_fs = compile(GL_FRAGMENT_SHADER, fetch_fragment_source);
        GLuint fetch_prog = glCreateProgram();
        glAttachShader(fetch_prog, vertex);
        glAttachShader(fetch_prog, fetch_fs);
        glLinkProgram(fetch_prog);
        glUseProgram(fetch_prog);
        glUniform1i(glGetUniformLocation(fetch_prog, "tex"), 0);
        static const struct {
            const char *name;
            GLenum internal_format, format, type;
            unsigned char bytes[4];
            GLint swizzle[4];
            unsigned char expected[4];
        } fetches[] = {
            /* The CTS's own case: R8 read through a reversed swizzle. R8
             * samples as (v,0,0,1), so red takes alpha's 1.0 and alpha takes
             * red's value. */
            {"fetch_r8_reversed", GL_R8, GL_RED, GL_UNSIGNED_BYTE, {150},
             {GL_ALPHA, GL_BLUE, GL_GREEN, GL_RED}, {255, 0, 0, 150}},
            {"fetch_rgba8_reversed", GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE,
             {40, 80, 120, 200},
             {GL_ALPHA, GL_BLUE, GL_GREEN, GL_RED}, {200, 120, 80, 40}},
            {"fetch_r8_broadcast", GL_R8, GL_RED, GL_UNSIGNED_BYTE, {150},
             {GL_RED, GL_RED, GL_RED, GL_ONE}, {150, 150, 150, 255}},
        };
        for (unsigned i = 0; i < sizeof(fetches) / sizeof(fetches[0]); ++i) {
            GLuint texture = 0;
            glGenTextures(1, &texture);
            glBindTexture(GL_TEXTURE_2D, texture);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
            glTexImage2D(GL_TEXTURE_2D, 0, fetches[i].internal_format, 1, 1, 0,
                         fetches[i].format, fetches[i].type, fetches[i].bytes);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_R,
                            fetches[i].swizzle[0]);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_G,
                            fetches[i].swizzle[1]);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_B,
                            fetches[i].swizzle[2]);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_A,
                            fetches[i].swizzle[3]);
            glClearColor(0.0f, 0.0f, 0.0f, 0.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLES, 0, 6);
            unsigned char got[4] = {0xAB, 0xAB, 0xAB, 0xAB};
            glReadPixels(2, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
            const GLenum error = glGetError();
            int ok = error == GL_NO_ERROR;
            for (unsigned c = 0; c < 4; ++c) {
                const int delta = (int)got[c] - (int)fetches[i].expected[c];
                if (delta > 2 || delta < -2) ok = 0;
            }
            fprintf(stderr, "INFO %s got=%u,%u,%u,%u expected=%u,%u,%u,%u "
                    "err=0x%04x\n", fetches[i].name, got[0], got[1], got[2],
                    got[3], fetches[i].expected[0], fetches[i].expected[1],
                    fetches[i].expected[2], fetches[i].expected[3],
                    (unsigned)error);
            result(fetches[i].name, ok);
            /* Deliberately not deleted: recycling the name would let a stale
             * view be blamed on the wrong thing. */
        }
        glUseProgram(program);
    }

    /* The CTS functional case end to end: R8 source with a reversed swizzle,
     * read by texelFetch, rendered into an *R8* output texture and read back
     * with glGetTexImage. Each half of this passes on its own above; this is
     * the combination the matrix actually runs. */
    {
        GLuint fetch_fs = compile(GL_FRAGMENT_SHADER, fetch_fragment_source);
        GLuint fetch_prog = glCreateProgram();
        glAttachShader(fetch_prog, vertex);
        glAttachShader(fetch_prog, fetch_fs);
        glLinkProgram(fetch_prog);
        glUseProgram(fetch_prog);
        glUniform1i(glGetUniformLocation(fetch_prog, "tex"), 0);

        GLuint src = 0;
        glGenTextures(1, &src);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, src);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
        /* Allocate empty, then upload -- the order the CTS uses
         * (texImage2D with NULL in prepareSourceTexture, texSubImage2D in
         * fillSourceTexture). Passing the data straight to texImage2D is a
         * different path through the driver. */
        const unsigned char src_byte = 150;
        glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, 1, 1, 0, GL_RED,
                     GL_UNSIGNED_BYTE, NULL);
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 1, 1, GL_RED,
                        GL_UNSIGNED_BYTE, &src_byte);
        /* The combined setter, which is what the CTS functional matrix uses
         * (gl3cTextureSwizzleTests.cpp:2654) when its use_rgba_enum path is
         * taken. Every other case here sets the four states individually. */
        const GLint swizzle_rgba[4] = {GL_ALPHA, GL_BLUE, GL_GREEN, GL_RED};
        glTexParameteriv(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_RGBA, swizzle_rgba);

        GLuint out_tex = 0, out_fb = 0;
        glGenTextures(1, &out_tex);
        glBindTexture(GL_TEXTURE_2D, out_tex);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, 1, 1, 0, GL_RED,
                     GL_UNSIGNED_BYTE, NULL);
        glGenFramebuffers(1, &out_fb);
        glBindFramebuffer(GL_FRAMEBUFFER, out_fb);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, out_tex, 0);
        const GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
        glViewport(0, 0, 1, 1);
        glBindTexture(GL_TEXTURE_2D, src);
        glClearColor(0.0f, 0.0f, 0.0f, 0.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        const GLenum clear_error = glGetError();
        glDrawArrays(GL_TRIANGLES, 0, 6);
        unsigned char back[4];
        memset(back, 0xAB, sizeof(back));
        glBindTexture(GL_TEXTURE_2D, out_tex);
        glGetTexImage(GL_TEXTURE_2D, 0, GL_RED, GL_UNSIGNED_BYTE, back);
        const GLenum error = glGetError();
        /* R8 source samples as (v,0,0,1); swizzle R<-ALPHA takes the 1.0. */
        int ok = error == GL_NO_ERROR && clear_error == GL_NO_ERROR &&
                 status == GL_FRAMEBUFFER_COMPLETE &&
                 back[0] >= 253;
        fprintf(stderr, "INFO cts_shape status=0x%04x clear_err=0x%04x "
                "err=0x%04x got=%u expected=255\n", (unsigned)status,
                (unsigned)clear_error, (unsigned)error, back[0]);
        result("cts_shape_r8_fetch_swizzle", ok);
        glUseProgram(program);
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glViewport(0, 0, 4, 4);
    }

    /* Change the swizzle on an *already drawn with* texture and draw again.
     * The CTS functional matrix does this -- one source texture, many swizzle
     * combinations, a draw between each -- while every case above uses a
     * fresh texture, which is the one thing left untested. */
    {
        GLuint fetch_fs = compile(GL_FRAGMENT_SHADER, fetch_fragment_source);
        GLuint fetch_prog = glCreateProgram();
        glAttachShader(fetch_prog, vertex);
        glAttachShader(fetch_prog, fetch_fs);
        glLinkProgram(fetch_prog);
        glUseProgram(fetch_prog);
        glUniform1i(glGetUniformLocation(fetch_prog, "tex"), 0);
        GLuint src = 0;
        glGenTextures(1, &src);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, src);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
        const unsigned char texel[4] = {40, 80, 120, 200};
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                     GL_UNSIGNED_BYTE, texel);
        static const struct {
            const char *name;
            GLint swizzle[4];
            unsigned char expected[4];
        } steps[] = {
            {"resswizzle_identity", {GL_RED, GL_GREEN, GL_BLUE, GL_ALPHA},
             {40, 80, 120, 200}},
            {"resswizzle_reversed", {GL_ALPHA, GL_BLUE, GL_GREEN, GL_RED},
             {200, 120, 80, 40}},
            {"resswizzle_broadcast", {GL_GREEN, GL_GREEN, GL_GREEN, GL_ONE},
             {80, 80, 80, 255}},
        };
        for (unsigned i = 0; i < sizeof(steps) / sizeof(steps[0]); ++i) {
            glBindTexture(GL_TEXTURE_2D, src);
            glTexParameteriv(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_RGBA,
                             steps[i].swizzle);
            glClearColor(0.0f, 0.0f, 0.0f, 0.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLES, 0, 6);
            unsigned char got[4] = {0xAB, 0xAB, 0xAB, 0xAB};
            glReadPixels(2, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
            const GLenum error = glGetError();
            int ok = error == GL_NO_ERROR;
            for (unsigned c = 0; c < 4; ++c) {
                const int delta = (int)got[c] - (int)steps[i].expected[c];
                if (delta > 2 || delta < -2) ok = 0;
            }
            fprintf(stderr, "INFO %s got=%u,%u,%u,%u expected=%u,%u,%u,%u "
                    "err=0x%04x\n", steps[i].name, got[0], got[1], got[2],
                    got[3], steps[i].expected[0], steps[i].expected[1],
                    steps[i].expected[2], steps[i].expected[3],
                    (unsigned)error);
            result(steps[i].name, ok);
        }
        glUseProgram(program);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
