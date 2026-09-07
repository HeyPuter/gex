/* Stencil uploaded per layer into a layered depth-stencil texture.
 *
 * glTexSubImage3D on a GL_DEPTH32F_STENCIL8 array texture has to put each
 * slice's stencil in that slice. The driver here wrote the *depth* half to the
 * requested layer -- its render pass views baseArrayLayer = box->z -- and the
 * stencil half through a queue write whose origin.z was never set, so every
 * slice's stencil landed in slice zero. Silently: no error, no message, and
 * three of four slices holding whatever slice zero got.
 *
 * A stencil test is the check, because that is what the data is *for* and what
 * an application would notice. Each layer is uploaded a distinct reference
 * value, then attached on its own and drawn against GL_EQUAL: the fragment
 * survives only if that layer's stencil is what was uploaded to it. Layer zero
 * would pass even with the bug, so the later layers are the ones that matter
 * and they are checked separately rather than as a total.
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

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

#define W 8
#define H 8
#define LAYERS 4

static const char *vs_src =
    "#version 400\n"
    "void main() {\n"
    "    gl_Position = vec4((gl_VertexID == 1) ? 3.0 : -1.0,\n"
    "                       (gl_VertexID == 2) ? 3.0 : -1.0, 0.0, 1.0);\n"
    "}\n";
static const char *fs_src =
    "#version 400\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(1.0, 0.0, 0.0, 1.0); }\n";

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

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    eq("program_links", linked, 1);
    if (!linked) { printf("SUMMARY cases=%d passed=%d failed=%d\n", cases,
                          passed, cases - passed); return 1; }
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);

    /* A layered colour target to draw into, one slice per layer. */
    GLuint colour = 0; glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D_ARRAY, colour);
    glTexImage3D(GL_TEXTURE_2D_ARRAY, 0, GL_RGBA8, W, H, LAYERS, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);

    /* The layered depth-stencil texture, each slice uploaded its own stencil
       value. GL_FLOAT_32_UNSIGNED_INT_24_8_REV is eight bytes per texel: a
       float depth then the stencil byte in the next word's low byte. */
    GLuint ds = 0; glGenTextures(1, &ds);
    glBindTexture(GL_TEXTURE_2D_ARRAY, ds);
    glTexImage3D(GL_TEXTURE_2D_ARRAY, 0, GL_DEPTH32F_STENCIL8, W, H, LAYERS,
                 0, GL_DEPTH_STENCIL, GL_FLOAT_32_UNSIGNED_INT_24_8_REV, NULL);
    eq("layered_depth_stencil_created", drain(), GL_NO_ERROR);

    static const unsigned char refs[LAYERS] = { 1, 2, 3, 4 };
    for (unsigned l = 0; l < LAYERS; ++l) {
        struct { float depth; unsigned int stencil; } texels[W * H];
        for (unsigned i = 0; i < W * H; ++i) {
            texels[i].depth = 0.5f;
            texels[i].stencil = refs[l];
        }
        glTexSubImage3D(GL_TEXTURE_2D_ARRAY, 0, 0, 0, (GLint)l, W, H, 1,
                        GL_DEPTH_STENCIL, GL_FLOAT_32_UNSIGNED_INT_24_8_REV,
                        texels);
    }
    eq("per_layer_stencil_upload_is_clean", drain(), GL_NO_ERROR);

    GLuint fbo = 0; glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glViewport(0, 0, W, H);
    glUseProgram(program);
    glEnable(GL_STENCIL_TEST);
    glStencilOp(GL_KEEP, GL_KEEP, GL_KEEP);
    glDisable(GL_DEPTH_TEST);

    for (unsigned l = 0; l < LAYERS; ++l) {
        /* This slice, and only this slice, of both attachments. */
        glFramebufferTextureLayer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                  colour, 0, (GLint)l);
        glFramebufferTextureLayer(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT,
                                  ds, 0, (GLint)l);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) !=
            GL_FRAMEBUFFER_COMPLETE) {
            fprintf(stderr, "INFO layer %u: framebuffer incomplete\n", l);
            eq("framebuffer_complete", 0, 1);
            break;
        }
        /* Clear colour only -- clearing stencil would destroy what was
           uploaded, which is the thing under test. */
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);

        /* Passes only where the stencil holds this layer's uploaded value. */
        glStencilFunc(GL_EQUAL, refs[l], 0xFF);
        glDrawArrays(GL_TRIANGLES, 0, 3);

        unsigned char pixel[4] = {0};
        glReadPixels(W / 2, H / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
        drain();
        char name[64];
        snprintf(name, sizeof name, "layer_%u_stencil_is_%u", l, refs[l]);
        /* 255 means this slice's stencil really held its own value. Under the
           bug every slice held slice zero's, so layer 0 passed and the rest
           came back black. */
        eq(name, pixel[0], 255);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
