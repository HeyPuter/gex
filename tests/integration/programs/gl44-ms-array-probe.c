/* GL_TEXTURE_2D_MULTISAMPLE_ARRAY, split into the half that might work and
 * the half that cannot.
 *
 * WGSL has `texture_multisampled_2d` and no array form, so a shader that
 * declares sampler2DMSArray is refused outright:
 *
 *   gex-wgpu: shader rejected: gex_wgpu a multisampled array texture,
 *             which WGSL has no type for
 *
 * That single refusal is the largest cluster in the whole CTS -- 62 cases of
 * texture_swizzle, 30 of direct_state_access and 4 of texture_view, 96 in all.
 * Before any of it can be served, one thing has to be established that the
 * refusal above hides: whether the *texture* exists. The shader is rejected
 * before anything touches the allocation, so "no resource refusal was
 * printed" is not evidence either way.
 *
 * It decides between two emulations of very different cost. If WebGPU will
 * create a multisampled array texture, then each layer has a plain
 * `texture_multisampled_2d` view and the layer index can be unrolled the way
 * sampler arrays already are -- no change to storage at all. If it will not,
 * the layers have to be tiled side by side into one wide multisampled 2D
 * texture, with every coordinate, viewport and copy offset by the layer.
 *
 * So this probe never mentions sampler2DMSArray. It renders and resolves,
 * which is all the rendering half needs:
 *
 *   create_ms_array       the texture is created without a GL error
 *   fbo_layer_complete    one layer attaches and the framebuffer is complete
 *   clear_reaches_layer   clearing that layer and resolving it reads back
 *   layers_are_distinct   two layers cleared differently stay different,
 *                         which is what a single shared allocation fails
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

#define SIDE 8
#define LAYERS 3
#define SAMPLES 4

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLuint compile(GLenum stage, const char *source)
{
    GLuint shader = glCreateShader(stage);
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
    return shader;
}

/* Clears layer `layer` to `r`, then resolves it into a single-sample
 * framebuffer and reads the centre pixel back. */
static int clear_and_read(GLuint ms_tex, unsigned layer, float r,
                          unsigned char out[4])
{
    GLuint fbo = 0;
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTextureLayer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, ms_tex, 0,
                              (GLint)layer);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        glDeleteFramebuffers(1, &fbo);
        return 0;
    }
    glViewport(0, 0, SIDE, SIDE);
    glClearColor(r, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);

    GLuint flat_tex = 0, flat_fbo = 0;
    glGenTextures(1, &flat_tex);
    glBindTexture(GL_TEXTURE_2D, flat_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &flat_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, flat_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           flat_tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        glDeleteFramebuffers(1, &fbo);
        glDeleteFramebuffers(1, &flat_fbo);
        return 0;
    }
    /* The resolve, which is the only way to read a multisample attachment
     * without a sampler2DMSArray. */
    glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, flat_fbo);
    glBlitFramebuffer(0, 0, SIDE, SIDE, 0, 0, SIDE, SIDE,
                      GL_COLOR_BUFFER_BIT, GL_NEAREST);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, flat_fbo);
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, out);
    const GLenum err = glGetError();
    glDeleteFramebuffers(1, &fbo);
    glDeleteFramebuffers(1, &flat_fbo);
    glDeleteTextures(1, &flat_tex);
    return err == GL_NO_ERROR;
}

/* A real draw into one layer, and a real sampler2DMSArray read of it -- the
 * shape KHR-GL45.texture_swizzle and direct_state_access actually use, and
 * the half a clear-and-resolve cannot reach. Each sample is fetched by index
 * so a resolve cannot hide a per-sample error. */
static const char *draw_vs =
    "#version 400 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char *draw_fs =
    "#version 400 core\n"
    "layout(location = 0) out vec4 colour;\n"
    "void main() { colour = vec4(0.25, 0.5, 0.75, 1.0); }\n";

static const char *read_vs =
    "#version 400 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
/* Reads the four samples of one texel of one layer and packs them into the
 * channels, so a wrong sample index or a wrong layer shows up as a specific
 * channel rather than as a general smear. */
static const char *read_fs =
    "#version 400 core\n"
    "uniform sampler2DMSArray src;\n"
    "uniform int layer;\n"
    "layout(location = 0) out vec4 colour;\n"
    "void main() {\n"
    "  ivec3 at = ivec3(ivec2(gl_FragCoord.xy), layer);\n"
    "  colour = vec4(texelFetch(src, at, 0).x, texelFetch(src, at, 1).y,\n"
    "                texelFetch(src, at, 2).z, texelFetch(src, at, 3).w);\n"
    "}\n";

int main(void)
{
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(dpy, NULL, NULL)) { printf("FATAL eglInitialize\n"); return 1; }
    static const EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                       EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                                       EGL_NONE};
    EGLConfig cfg; EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, NULL);
    if (ctx == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 1; }
    static const EGLint surf_attrs[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, surf_attrs);
    eglMakeCurrent(dpy, surf, surf, ctx);

    while (glGetError() != GL_NO_ERROR) { }

    GLuint ms_tex = 0;
    glGenTextures(1, &ms_tex);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE_ARRAY, ms_tex);
    glTexImage3DMultisample(GL_TEXTURE_2D_MULTISAMPLE_ARRAY, SAMPLES, GL_RGBA8,
                            SIDE, SIDE, LAYERS, GL_TRUE);
    const GLenum create_err = glGetError();
    fprintf(stderr, "INFO glTexImage3DMultisample err=0x%04x\n",
            (unsigned)create_err);
    result("create_ms_array", create_err == GL_NO_ERROR);

    GLuint probe_fbo = 0;
    glGenFramebuffers(1, &probe_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, probe_fbo);
    glFramebufferTextureLayer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, ms_tex, 0, 1);
    const GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    fprintf(stderr, "INFO fbo status=0x%04x (complete is 0x%04x)\n",
            (unsigned)status, (unsigned)GL_FRAMEBUFFER_COMPLETE);
    result("fbo_layer_complete", status == GL_FRAMEBUFFER_COMPLETE);
    glDeleteFramebuffers(1, &probe_fbo);

    unsigned char zero[4] = {0};
    /* Layer 0 first, whose tile offset is zero: if this one reads back and
     * the others do not, the fault is in the offset rather than in the clear
     * or the resolve. */
    const int got_zero = clear_and_read(ms_tex, 0, 0.5f, zero);
    fprintf(stderr, "INFO layer0=%u,%u,%u,%u ok=%d\n", zero[0], zero[1],
            zero[2], zero[3], got_zero);
    unsigned char one[4] = {0}, two[4] = {0};
    /* 0.25 and 0.75 rather than 0 and 1: a layer that reads back as the clear
     * colour of the *other* layer is the failure this is looking for, and two
     * mid values cannot be confused with an untouched or saturated buffer. */
    const int got_one = clear_and_read(ms_tex, 1, 0.25f, one);
    const int got_two = clear_and_read(ms_tex, 2, 0.75f, two);
    fprintf(stderr, "INFO layer1=%u,%u,%u,%u layer2=%u,%u,%u,%u\n",
            one[0], one[1], one[2], one[3], two[0], two[1], two[2], two[3]);

    result("clear_reaches_layer",
           got_one && one[0] > 55 && one[0] < 73);
    result("layers_are_distinct",
           got_one && got_two && two[0] > 180 && two[0] < 205 &&
               one[0] != two[0]);

    /* Draw into layer 1, then read its four samples back by index. */
    GLuint draw_prog = 0, read_prog = 0, quad = 0;
    {
        GLuint v = compile(GL_VERTEX_SHADER, draw_vs);
        GLuint f = compile(GL_FRAGMENT_SHADER, draw_fs);
        if (v && f) {
            draw_prog = glCreateProgram();
            glAttachShader(draw_prog, v); glAttachShader(draw_prog, f);
            glBindAttribLocation(draw_prog, 0, "pos");
            glLinkProgram(draw_prog);
        }
        v = compile(GL_VERTEX_SHADER, read_vs);
        f = compile(GL_FRAGMENT_SHADER, read_fs);
        if (v && f) {
            read_prog = glCreateProgram();
            glAttachShader(read_prog, v); glAttachShader(read_prog, f);
            glBindAttribLocation(read_prog, 0, "pos");
            glLinkProgram(read_prog);
            GLint ok = 0;
            glGetProgramiv(read_prog, GL_LINK_STATUS, &ok);
            if (!ok) {
                char log[900] = {0};
                glGetProgramInfoLog(read_prog, sizeof(log) - 1, NULL, log);
                fprintf(stderr, "INFO read link: %s\n", log);
                read_prog = 0;
            }
        }
        static const float verts[] = {-1, -1, -1, 1, 1, -1, 1, 1};
        glGenBuffers(1, &quad);
        glBindBuffer(GL_ARRAY_BUFFER, quad);
        glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts, GL_STATIC_DRAW);
    }
    unsigned char sampled[4] = {0};
    int drew = 0;
    if (draw_prog && read_prog) {
        GLuint fbo = 0;
        glGenFramebuffers(1, &fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glFramebufferTextureLayer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, ms_tex,
                                  0, 1);
        glViewport(0, 0, SIDE, SIDE);
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(draw_prog);
        glBindBuffer(GL_ARRAY_BUFFER, quad);
        glEnableVertexAttribArray(0);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

        GLuint flat_tex = 0, flat_fbo = 0;
        glGenTextures(1, &flat_tex);
        glBindTexture(GL_TEXTURE_2D, flat_tex);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                     GL_UNSIGNED_BYTE, NULL);
        glGenFramebuffers(1, &flat_fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, flat_fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, flat_tex, 0);
        glViewport(0, 0, SIDE, SIDE);
        glUseProgram(read_prog);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D_MULTISAMPLE_ARRAY, ms_tex);
        glUniform1i(glGetUniformLocation(read_prog, "src"), 0);
        glUniform1i(glGetUniformLocation(read_prog, "layer"), 1);
        glBindBuffer(GL_ARRAY_BUFFER, quad);
        glEnableVertexAttribArray(0);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                     sampled);
        drew = glGetError() == GL_NO_ERROR;
    }
    fprintf(stderr, "INFO drawn+sampled=%u,%u,%u,%u ok=%d\n", sampled[0],
            sampled[1], sampled[2], sampled[3], drew);
    /* 0.25, 0.5, 0.75, 1.0 written by the draw, one channel per sample. */
    result("draw_then_sample_layer",
           drew && sampled[0] > 55 && sampled[0] < 73 &&
               sampled[1] > 120 && sampled[1] < 138 &&
               sampled[2] > 180 && sampled[2] < 200 && sampled[3] > 250);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
