/* A vertex shader reading an imageBuffer.
 *
 * WGSL has no buffer storage texture, so this driver serves an imageBuffer
 * through a storage buffer the shader addresses itself -- the "packed" route.
 * The *fragment* group carried bindings for those; the vertex group did not,
 * at all. So the module declared a binding that lived in no group, Dawn said
 *
 *     No bind group set at group index 1
 *
 * and the draw rendered nothing while glGetError stayed clean. A GL capability
 * that silently draws nothing is the worst shape of failure, which is why this
 * is worth a probe of its own rather than a CTS case alone.
 *
 * Read-only on purpose: a *written* image in the vertex stage has its store
 * stripped from the rasterising copy, because WGSL forbids a writable storage
 * binding there, and the write is performed by a compute clone instead. What
 * the rasterising shader keeps is the read, and the read is what had nowhere
 * to bind.
 *
 * The value travels through a varying so the check is the number the shader
 * loaded, not merely that something drew: 0.5 arrives as 128, and a missing
 * binding reads zero, which is the failure being tested for.
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

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

static const char *vs_src =
    "#version 430\n"
    "layout(rg32f) readonly uniform imageBuffer g_image;\n"
    "out float carried;\n"
    "void main() {\n"
    "    carried = imageLoad(g_image, gl_VertexID).x;\n"
    "    gl_Position = vec4((gl_VertexID == 1) ? 3.0 : -1.0,\n"
    "                       (gl_VertexID == 2) ? 3.0 : -1.0, 0.0, 1.0);\n"
    "}\n";
static const char *fs_src =
    "#version 430\n"
    "in float carried;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(carried, 0.0, 0.0, 1.0); }\n";
/* And with a uniform block beside it, so the packed bindings have to sit at
   the right index rather than merely exist: the group's entries are numbered
   uniform blocks, storage blocks, buffer textures, packed images, draw
   parameters, and getting that order wrong binds the header where a block
   belongs. */
static const char *vs_with_block =
    "#version 430\n"
    "layout(std140, binding = 0) uniform Scale { vec4 factor; };\n"
    "layout(rg32f) readonly uniform imageBuffer g_image;\n"
    "out float carried;\n"
    "void main() {\n"
    "    carried = imageLoad(g_image, gl_VertexID).x * factor.x;\n"
    "    gl_Position = vec4((gl_VertexID == 1) ? 3.0 : -1.0,\n"
    "                       (gl_VertexID == 2) ? 3.0 : -1.0, 0.0, 1.0);\n"
    "}\n";

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1400] = {0};
               glGetShaderInfoLog(sh, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s compile failed: %s\n", what, log); }
    return ok ? sh : 0;
}
static GLuint link2(const char *v, const char *what) {
    GLuint vs = compile(GL_VERTEX_SHADER, v, what);
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !fs) return 0;
    GLuint p = glCreateProgram();
    glAttachShader(p, vs); glAttachShader(p, fs);
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) { char log[1400] = {0};
               glGetProgramInfoLog(p, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s link failed: %s\n", what, log);
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
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);

    /* A texture buffer over a real buffer, bound as the image. */
    GLuint buf = 0; glGenBuffers(1, &buf);
    glBindBuffer(GL_TEXTURE_BUFFER, buf);
    const float data[8] = { 0.5f, 0.0f, 0.5f, 0.0f, 0.5f, 0.0f, 0.5f, 0.0f };
    glBufferData(GL_TEXTURE_BUFFER, sizeof data, data, GL_STATIC_DRAW);
    GLuint tex = 0; glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_BUFFER, tex);
    glTexBuffer(GL_TEXTURE_BUFFER, GL_RG32F, buf);
    glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_ONLY, GL_RG32F);
    eq("binding_the_image_is_clean", drain(), GL_NO_ERROR);

    GLuint colour = 0; glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 8, 8);
    GLuint fbo = 0; glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    glViewport(0, 0, 8, 8);

    GLuint plain = link2(vs_src, "plain");
    eq("vertex_imagebuffer_links", plain != 0, 1);
    if (plain) {
        glUseProgram(plain);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        eq("vertex_imagebuffer_draw_is_clean", drain(), GL_NO_ERROR);
        unsigned char pixel[4] = {0};
        glReadPixels(4, 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
        drain();
        /* 0.5 -> 128. Zero is the missing-binding failure. */
        eq("vertex_imagebuffer_value_arrives", pixel[0], 128);
    }

    /* Again with a uniform block, so the packed entries must be numbered
       after it rather than over it. */
    GLuint ubo = 0; glGenBuffers(1, &ubo);
    const float factor[4] = { 1.0f, 0.0f, 0.0f, 0.0f };
    glBindBuffer(GL_UNIFORM_BUFFER, ubo);
    glBufferData(GL_UNIFORM_BUFFER, sizeof factor, factor, GL_STATIC_DRAW);
    glBindBufferBase(GL_UNIFORM_BUFFER, 0, ubo);
    GLuint blocked = link2(vs_with_block, "with a block");
    eq("with_a_uniform_block_links", blocked != 0, 1);
    if (blocked) {
        glUseProgram(blocked);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        eq("with_a_uniform_block_is_clean", drain(), GL_NO_ERROR);
        unsigned char pixel[4] = {0};
        glReadPixels(4, 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
        drain();
        eq("with_a_uniform_block_value_arrives", pixel[0], 128);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
