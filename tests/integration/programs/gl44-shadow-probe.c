/* GL's old-style shadow sampling, which returns a vec4.
 *
 * `shadow2D` and `shadow2DProj` are GLSL 1.20 functions and they return a
 * vec4, where `texture(sampler2DShadow, ...)` returns a scalar. SPIR-V has
 * only the scalar form -- `OpImageSample*Dref*` produces one -- so a driver
 * built on it has to sample as new-style and expand the result back out. Get
 * that wrong and the shader does not compile at all: Tint answers an
 * `OpImageSampleProjDrefImplicitLod` with a vec4 result type with
 *
 *   error: Expected Result Type to be int or float scalar type
 *
 * which is how `shadowtex` failed. This is also not a museum piece: every
 * GL2-era shadow map is written this way.
 *
 * The texture is 2x2 with four different depths, sampled at four points with
 * a reference value between them, so a comparison that ignores the texel --
 * or reads the wrong one -- fails rather than passing by luck. The alpha
 * channel is checked separately, because the expansion is (r, r, r, 1) and a
 * (r, r, r, r) expansion is right in the passing case and wrong in the
 * failing one.
 *
 * A compatibility profile is required for the legacy functions, which is what
 * an empty context attribute list asks for.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* Two shader sets, because the legacy functions need a compatibility
 * profile and not every EGL here hands one out -- native Mesa answers an
 * empty context attribute list with 4.6 Compatibility, while the guest's EGL
 * gives 3.3 Core. The core set uses texture()/textureProj() on a
 * sampler2DShadow, which return a scalar and are the GLSL 1.30 spelling of
 * the same lookups; the legacy set uses shadow2D/shadow2DProj, which return a
 * vec4. Every case below exists in both, so the case names and the count do
 * not depend on which profile turned up and one gate covers both. */

/* No #version: GLSL 1.10/1.20, which is where shadow2D lives. */
static const char *legacy_vertex_source =
    "varying vec4 coord;\n"
    "attribute vec2 pos;\n"
    "attribute vec4 shadowcoord;\n"
    "void main() {\n"
    "    coord = shadowcoord;\n"
    "    gl_Position = vec4(pos, 0.0, 1.0);\n"
    "}\n";

/* shadow2D: the plain form, coordinate already divided. */
static const char *legacy_shadow2d_source =
    "uniform sampler2DShadow depth;\n"
    "varying vec4 coord;\n"
    "void main() { gl_FragColor = shadow2D(depth, coord.xyz); }\n";

/* shadow2DProj: the form whose vec4 result rejected the whole shader. q
 * divides s, t and r, so passing 2.0 in q and doubling the other three asks
 * for the same texel and the same comparison as the case above -- which is
 * the point: the two must agree. */
static const char *legacy_shadow2dproj_source =
    "uniform sampler2DShadow depth;\n"
    "varying vec4 coord;\n"
    "void main() {\n"
    "    vec4 scaled = vec4(coord.xyz * 2.0, 2.0);\n"
    "    gl_FragColor = shadow2DProj(depth, scaled);\n"
    "}\n";

static const char *legacy_plain_source =
    "uniform sampler2D depth;\n"
    "varying vec4 coord;\n"
    "void main() { gl_FragColor = texture2D(depth, coord.xy); }\n";

static const char *core_vertex_source =
    "#version 330 core\n"
    "out vec4 coord;\n"
    "in vec2 pos;\n"
    "in vec4 shadowcoord;\n"
    "void main() {\n"
    "    coord = shadowcoord;\n"
    "    gl_Position = vec4(pos, 0.0, 1.0);\n"
    "}\n";

static const char *core_shadow2d_source =
    "#version 330 core\n"
    "uniform sampler2DShadow depth;\n"
    "in vec4 coord;\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = vec4(texture(depth, coord.xyz)); }\n";

/* textureProj on a sampler2DShadow: core GLSL, and the reference value is
 * still coord.p / coord.q, so the projective divide has to reach it. */
static const char *core_shadow2dproj_source =
    "#version 330 core\n"
    "uniform sampler2DShadow depth;\n"
    "in vec4 coord;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    vec4 scaled = vec4(coord.xyz * 2.0, 2.0);\n"
    "    fragColour = vec4(textureProj(depth, scaled));\n"
    "}\n";

static const char *core_plain_source =
    "#version 330 core\n"
    "uniform sampler2D depth;\n"
    "in vec4 coord;\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = texture(depth, coord.xy); }\n";

/* Chosen once the profile is known. */
static const char *vertex_source;
static const char *shadow2d_source;
static const char *shadow2dproj_source;
static const char *plain_source;

/* Red and alpha only, deliberately.
 *
 * How a depth texture's green and blue read is a profile difference, not a
 * driver one: GL_DEPTH_TEXTURE_MODE exists only in a compatibility profile,
 * where its default of GL_LUMINANCE makes the lookup (d, d, d, 1), while a
 * core profile has no such state and reads (d, 0, 0, 1). Native Mesa answers
 * an empty context attribute list with 4.6 Compatibility and the guest's EGL
 * with 3.3 Core, so this probe sees both and must not assert either.
 *
 * Alpha is still checked, because it is the channel that distinguishes the
 * (r, r, r, 1) expansion of an old-style shadow lookup from an (r, r, r, r)
 * one -- and the swizzle itself is gl44-swizzle-probe's 18 cases. */
static int channels_match(const unsigned char *got, const unsigned char *want)
{
    return (int)got[0] - (int)want[0] <= 2 &&
           (int)want[0] - (int)got[0] <= 2 &&
           (int)got[3] - (int)want[3] <= 2 &&
           (int)want[3] - (int)got[3] <= 2;
}

static GLuint compile(GLenum stage, const char *source, const char *name) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s compile failed: %s\n", name, log);
        return 0;
    }
    return shader;
}

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
    /* Empty: whatever the driver offers by default, which is a compatibility
     * profile. Asking for 3.3 core would reject shadow2D at compile time. */
    const EGLint context_attributes[] = {EGL_NONE};
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    /* Which spelling this profile accepts, decided by compiling one legacy
     * fragment shader rather than by parsing the version string: a core
     * profile rejects `varying`, and an implementation that tolerates it
     * anyway should be tested through the legacy path. */
    GLuint probe_shader = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(probe_shader, 1, &legacy_shadow2d_source, NULL);
    glCompileShader(probe_shader);
    GLint legacy_ok = 0;
    glGetShaderiv(probe_shader, GL_COMPILE_STATUS, &legacy_ok);
    glDeleteShader(probe_shader);
    while (glGetError() != GL_NO_ERROR) { }
    vertex_source = legacy_ok ? legacy_vertex_source : core_vertex_source;
    shadow2d_source =
        legacy_ok ? legacy_shadow2d_source : core_shadow2d_source;
    shadow2dproj_source =
        legacy_ok ? legacy_shadow2dproj_source : core_shadow2dproj_source;
    plain_source = legacy_ok ? legacy_plain_source : core_plain_source;
    fprintf(stderr, "INFO shadow_form=%s\n", legacy_ok ? "legacy" : "core");

    /* A core profile has no default vertex array object, and glDrawArrays
     * without one bound is GL_INVALID_OPERATION -- which is how every case
     * here came back as the clear colour with err=0x0502 in the guest, whose
     * EGL hands out 3.3 Core where native Mesa hands out Compatibility.
     * Harmless in a compatibility profile. */
    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* A 1x1 colour target: every fragment of the draw covers it, and all
     * four sample points are asked for in four separate draws instead. */
    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 1, 1);

    /* 2x2 depths, all distinct and none at 0.5, which is the reference the
     * cases below compare against. */
    static const float depths[4] = {
        0.2f, 0.8f,   /* (0,0) (1,0) */
        0.3f, 0.9f,   /* (0,1) (1,1) */
    };
    GLuint depth_texture = 0;
    glGenTextures(1, &depth_texture);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, depth_texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_MODE,
                    GL_COMPARE_REF_TO_TEXTURE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_FUNC, GL_LEQUAL);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH_COMPONENT24, 2, 2, 0,
                 GL_DEPTH_COMPONENT, GL_FLOAT, depths);
    if (glGetError() != GL_NO_ERROR) {
        fprintf(stderr, "INFO depth texture allocation failed\n");
        result("depth_texture", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }
    result("depth_texture", 1);

    static const float quad[8] = {
        -1.0f, -1.0f,  3.0f, -1.0f,  -1.0f, 3.0f,  3.0f, 3.0f,
    };
    GLuint position_buffer = 0;
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);

    struct {
        const char *name;
        const char *source;
        /* (s, t, r) before any projection. */
        float s, t, r;
        unsigned char want[4];
    } tests[] = {
        /* r = 0.5 against texel (0,0) = 0.2: LEQUAL is false, so 0. */
        {"shadow2D_fail", shadow2d_source, 0.25f, 0.25f, 0.5f, {0, 0, 0, 255}},
        /* r = 0.5 against texel (1,0) = 0.8: true, so 1. */
        {"shadow2D_pass", shadow2d_source, 0.75f, 0.25f, 0.5f,
         {255, 255, 255, 255}},
        /* The same two through the projective form, which must agree. */
        {"shadow2DProj_fail", shadow2dproj_source, 0.25f, 0.25f, 0.5f,
         {0, 0, 0, 255}},
        {"shadow2DProj_pass", shadow2dproj_source, 0.75f, 0.25f, 0.5f,
         {255, 255, 255, 255}},
        /* The other row, so a lookup that ignores t is caught: texel (0,1) is
         * 0.3 (false) and (1,1) is 0.9 (true). */
        {"shadow2D_row1_fail", shadow2d_source, 0.25f, 0.75f, 0.5f,
         {0, 0, 0, 255}},
        {"shadow2D_row1_pass", shadow2d_source, 0.75f, 0.75f, 0.5f,
         {255, 255, 255, 255}},
    };

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source, "vertex");
    if (!vertex) {
        result("vertex_shader", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        GLuint fs = compile(GL_FRAGMENT_SHADER, tests[i].source, tests[i].name);
        if (!fs) { result(tests[i].name, 0); continue; }
        GLuint program = glCreateProgram();
        glAttachShader(program, vertex);
        glAttachShader(program, fs);
        glLinkProgram(program);
        GLint linked = 0;
        glGetProgramiv(program, GL_LINK_STATUS, &linked);
        if (!linked) {
            char log[512] = {0};
            glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO %s link failed: %s\n", tests[i].name, log);
            result(tests[i].name, 0);
            continue;
        }
        glUseProgram(program);
        glUniform1i(glGetUniformLocation(program, "depth"), 0);

        const GLint position = glGetAttribLocation(program, "pos");
        const GLint shadowcoord = glGetAttribLocation(program, "shadowcoord");
        glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
        glEnableVertexAttribArray(position);
        glVertexAttribPointer(position, 2, GL_FLOAT, GL_FALSE, 0, NULL);
        /* One coordinate for the whole draw: a constant attribute, so there
         * is nothing for interpolation to get wrong. */
        glDisableVertexAttribArray(shadowcoord);
        glVertexAttrib4f(shadowcoord, tests[i].s, tests[i].t, tests[i].r, 1.0f);

        glClearColor(0.5f, 0.5f, 0.5f, 0.5f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

        unsigned char got[4] = {0xAB, 0xAB, 0xAB, 0xAB};
        glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
        const GLenum error = glGetError();
        const int ok = error == GL_NO_ERROR &&
                       channels_match(got, tests[i].want);
        fprintf(stderr, "INFO %s got=%u,%u,%u,%u want=%u,*,*,%u err=0x%04x\n",
                tests[i].name, got[0], got[1], got[2], got[3],
                tests[i].want[0], tests[i].want[3], (unsigned)error);
        result(tests[i].name, ok);
        glDeleteProgram(program);
        glDeleteShader(fs);
    }

    /* The other way a shadow map is made: rendered into rather than uploaded.
     *
     * Every real shadow map is built this way, and it is a different path
     * through the driver -- the depth texture is a render attachment first
     * and a sampled texture afterwards, and what the depth test wrote has to
     * be what the comparison reads. `shadowtex` renders its map into an FBO
     * exactly like this, so an upload-only probe would pass while the demo
     * showed no shadow at all.
     *
     * A quad at z = 0.4 in NDC, which is 0.7 in window depth, is drawn into a
     * 2x2 depth texture; a reference of 0.5 is then in front of it and 0.9
     * behind it. */
    GLuint rendered = 0, depth_fbo = 0;
    glGenTextures(1, &rendered);
    glBindTexture(GL_TEXTURE_2D, rendered);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_MODE,
                    GL_COMPARE_REF_TO_TEXTURE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_FUNC, GL_LEQUAL);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH_COMPONENT24, 2, 2, 0,
                 GL_DEPTH_COMPONENT, GL_FLOAT, NULL);
    glGenFramebuffers(1, &depth_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, depth_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D,
                           rendered, 0);
    glDrawBuffer(GL_NONE);
    const int depth_complete =
        glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
    result("depth_fbo_complete", depth_complete);
    if (depth_complete) {
        glViewport(0, 0, 2, 2);
        glEnable(GL_DEPTH_TEST);
        glDepthFunc(GL_ALWAYS);
        glDepthMask(GL_TRUE);
        glClearDepth(1.0);
        glClear(GL_DEPTH_BUFFER_BIT);
        /* A depth-only pass needs no colour shader, but a program has to be
         * bound; the shadow2D one writes to a target that is not there, which
         * is what GL_NONE above is for. */
        GLuint fill_vs = compile(GL_VERTEX_SHADER, legacy_ok
            ? "attribute vec2 pos;\n"
              "void main() { gl_Position = vec4(pos, 0.4, 1.0); }\n"
            : "#version 330 core\n"
              "in vec2 pos;\n"
              "void main() { gl_Position = vec4(pos, 0.4, 1.0); }\n",
            "fill_vs");
        GLuint fill_fs = compile(GL_FRAGMENT_SHADER, legacy_ok
            ? "void main() { gl_FragColor = vec4(1.0); }\n"
            : "#version 330 core\n"
              "out vec4 fragColour;\n"
              "void main() { fragColour = vec4(1.0); }\n",
            "fill_fs");
        GLuint fill = glCreateProgram();
        glAttachShader(fill, fill_vs);
        glAttachShader(fill, fill_fs);
        glLinkProgram(fill);
        glUseProgram(fill);
        const GLint fill_pos = glGetAttribLocation(fill, "pos");
        glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
        glEnableVertexAttribArray(fill_pos);
        glVertexAttribPointer(fill_pos, 2, GL_FLOAT, GL_FALSE, 0, NULL);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glDisable(GL_DEPTH_TEST);
        glDisableVertexAttribArray(fill_pos);

        /* Read the depth back, so a comparison failure can be told apart
         * from a map that was never written. */
        float readback[4] = {-1.0f, -1.0f, -1.0f, -1.0f};
        glReadPixels(0, 0, 2, 2, GL_DEPTH_COMPONENT, GL_FLOAT, readback);
        fprintf(stderr, "INFO rendered_depth=%.4f,%.4f,%.4f,%.4f err=0x%04x\n",
                readback[0], readback[1], readback[2], readback[3],
                (unsigned)glGetError());
        result("rendered_depth_is_0.7",
               readback[0] > 0.69f && readback[0] < 0.71f);

        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glViewport(0, 0, 1, 1);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, rendered);

        struct { const char *name; const char *source; float r;
                 unsigned char want[4]; } rendered_tests[] = {
            /* 0.5 <= 0.7: in front of the surface, so lit. */
            {"rendered_pass", shadow2d_source, 0.5f, {255, 255, 255, 255}},
            /* 0.9 <= 0.7 is false: behind it, so shadowed. */
            {"rendered_fail", shadow2d_source, 0.9f, {0, 0, 0, 255}},
            /* And through the projective form, which is what a shadow map's
             * texgen coordinates actually arrive as. */
            {"rendered_proj_pass", shadow2dproj_source, 0.5f,
             {255, 255, 255, 255}},
            {"rendered_proj_fail", shadow2dproj_source, 0.9f, {0, 0, 0, 255}},
        };
        for (unsigned i = 0;
             i < sizeof(rendered_tests) / sizeof(rendered_tests[0]); ++i) {
            GLuint fs = compile(GL_FRAGMENT_SHADER, rendered_tests[i].source,
                                rendered_tests[i].name);
            if (!fs) { result(rendered_tests[i].name, 0); continue; }
            GLuint program = glCreateProgram();
            glAttachShader(program, vertex);
            glAttachShader(program, fs);
            glLinkProgram(program);
            GLint linked = 0;
            glGetProgramiv(program, GL_LINK_STATUS, &linked);
            if (!linked) { result(rendered_tests[i].name, 0); continue; }
            glUseProgram(program);
            glUniform1i(glGetUniformLocation(program, "depth"), 0);
            const GLint position = glGetAttribLocation(program, "pos");
            const GLint shadowcoord =
                glGetAttribLocation(program, "shadowcoord");
            glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
            glEnableVertexAttribArray(position);
            glVertexAttribPointer(position, 2, GL_FLOAT, GL_FALSE, 0, NULL);
            glDisableVertexAttribArray(shadowcoord);
            glVertexAttrib4f(shadowcoord, 0.5f, 0.5f, rendered_tests[i].r,
                             1.0f);
            glClearColor(0.5f, 0.5f, 0.5f, 0.5f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            unsigned char got[4] = {0xAB, 0xAB, 0xAB, 0xAB};
            glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
            const GLenum error = glGetError();
            const int ok = error == GL_NO_ERROR &&
                           channels_match(got, rendered_tests[i].want);
            fprintf(stderr,
                    "INFO %s got=%u,%u,%u,%u want=%u,*,*,%u err=0x%04x\n",
                    rendered_tests[i].name, got[0], got[1], got[2], got[3],
                    rendered_tests[i].want[0], rendered_tests[i].want[3],
                    (unsigned)error);
            result(rendered_tests[i].name, ok);
            glDeleteProgram(program);
            glDeleteShader(fs);
        }

        /* The same texture read as an ordinary one, which is what
         * GL_TEXTURE_COMPARE_MODE = GL_NONE asks for. `shadowtex`'s "show
         * depth texture image" mode does exactly this, and it came out black
         * while every comparison above was right -- so the two paths are
         * worth separating: one is textureSampleCompare and the other is
         * textureSample on the same depth texture.
         *
         * GL_DEPTH_TEXTURE_MODE defaults to GL_LUMINANCE, so 0.7 arrives as
         * (0.7, 0.7, 0.7, 1). */
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_COMPARE_MODE, GL_NONE);
        GLuint plain_fs =
            compile(GL_FRAGMENT_SHADER, plain_source, "plain_depth");
        if (plain_fs) {
            GLuint program = glCreateProgram();
            glAttachShader(program, vertex);
            glAttachShader(program, plain_fs);
            glLinkProgram(program);
            GLint linked = 0;
            glGetProgramiv(program, GL_LINK_STATUS, &linked);
            if (!linked) {
                char log[512] = {0};
                glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
                fprintf(stderr, "INFO plain_depth link failed: %s\n", log);
                result("plain_depth_sample", 0);
            } else {
                glUseProgram(program);
                glUniform1i(glGetUniformLocation(program, "depth"), 0);
                const GLint position = glGetAttribLocation(program, "pos");
                const GLint shadowcoord =
                    glGetAttribLocation(program, "shadowcoord");
                glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
                glEnableVertexAttribArray(position);
                glVertexAttribPointer(position, 2, GL_FLOAT, GL_FALSE, 0,
                                      NULL);
                glDisableVertexAttribArray(shadowcoord);
                glVertexAttrib4f(shadowcoord, 0.5f, 0.5f, 0.0f, 1.0f);
                glClearColor(0.0f, 0.0f, 0.0f, 0.0f);
                glClear(GL_COLOR_BUFFER_BIT);
                glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
                unsigned char got[4] = {0xAB, 0xAB, 0xAB, 0xAB};
                glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
                const GLenum error = glGetError();
                /* 0.7 * 255 = 178.5, and the texture is 24-bit, so a
                 * couple of levels of slack. Red and alpha only, for the
                 * reason channels_match gives. */
                const int ok = error == GL_NO_ERROR && got[0] >= 176 &&
                               got[0] <= 181 && got[3] >= 253;
                fprintf(stderr,
                        "INFO plain_depth got=%u,%u,%u,%u want=178,*,*,255 "
                        "err=0x%04x\n", got[0], got[1], got[2], got[3],
                        (unsigned)error);
                result("plain_depth_sample", ok);
            }
        } else {
            result("plain_depth_sample", 0);
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
