/* Sampling the stencil aspect of a packed depth-stencil texture.
 *
 * GL_DEPTH_STENCIL_TEXTURE_MODE = GL_STENCIL_INDEX makes a usampler2D read
 * the stencil half of a combined texture (ARB_stencil_texturing). Gallium
 * spells the aspect as a separate stencil-only format, and there are *four*
 * such spellings -- X24S8, S8X24, X32_S8X24 and plain S8 -- so a driver
 * listing three of them works on one packed format and refuses the other,
 * which is what happened here: depth24-stencil8 sampled and
 * depth32float-stencil8 was refused outright.
 *
 * Both packed formats are asked, because that asymmetry is invisible from
 * either one alone.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

#define SIDE 16
#define STENCIL_VALUE 0x2a

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 330 core\n"
    "const vec2 quad[4] = vec2[4](vec2(-1,-1), vec2(1,-1), vec2(-1,1), vec2(1,1));\n"
    "void main() { gl_Position = vec4(quad[gl_VertexID], 0.0, 1.0); }\n";
static const char *fs_src =
    "#version 330 core\n"
    "uniform usampler2D stencil_tex;\n"
    "out uvec4 out_value;\n"
    "void main() { out_value = uvec4(texelFetch(stencil_tex, ivec2(0), 0).r,0,0,1); }\n";

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

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glUseProgram(prog);
    glUniform1i(glGetUniformLocation(prog, "stencil_tex"), 0);

    /* An integer colour target to read the sampled stencil back through
     * unchanged -- a unorm one would rescale it. */
    GLuint out_tex = 0, out_fbo = 0;
    glGenTextures(1, &out_tex);
    glBindTexture(GL_TEXTURE_2D, out_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R32UI, SIDE, SIDE, 0, GL_RED_INTEGER,
                 GL_UNSIGNED_INT, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &out_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, out_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           out_tex, 0);

    struct { const char *name; GLenum internal; } runs[] = {
        {"depth24_stencil8", GL_DEPTH24_STENCIL8},
        {"depth32f_stencil8", GL_DEPTH32F_STENCIL8},
    };

    for (unsigned i = 0; i < 2; ++i) {
        /* The packed texture, with its stencil half cleared by rendering --
         * the only way WebGPU writes a depth-stencil aspect. */
        GLuint tex = 0, fbo = 0;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexImage2D(GL_TEXTURE_2D, 0, runs[i].internal, SIDE, SIDE, 0,
                     GL_DEPTH_STENCIL,
                     runs[i].internal == GL_DEPTH24_STENCIL8
                        ? GL_UNSIGNED_INT_24_8
                        : GL_FLOAT_32_UNSIGNED_INT_24_8_REV,
                     NULL);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glGenFramebuffers(1, &fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT,
                               GL_TEXTURE_2D, tex, 0);
        glDrawBuffer(GL_NONE);
        glReadBuffer(GL_NONE);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
            fprintf(stderr, "INFO %s framebuffer incomplete\n", runs[i].name);
            result(runs[i].name, 0);
            continue;
        }
        glViewport(0, 0, SIDE, SIDE);
        glClearStencil(STENCIL_VALUE);
        glClearDepth(0.5);
        glClear(GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);

        /* Read the stencil aspect, not the depth one. */
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexParameteri(GL_TEXTURE_2D, GL_DEPTH_STENCIL_TEXTURE_MODE,
                        GL_STENCIL_INDEX);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, tex);

        glBindFramebuffer(GL_FRAMEBUFFER, out_fbo);
        glDrawBuffer(GL_COLOR_ATTACHMENT0);
        glViewport(0, 0, SIDE, SIDE);
        const GLuint zero[4] = {0, 0, 0, 0};
        glClearBufferuiv(GL_COLOR, 0, zero);
        glDisable(GL_DEPTH_TEST);
        glUseProgram(prog);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

        GLuint got = 0xffffffffu;
        glReadBuffer(GL_COLOR_ATTACHMENT0);
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RED_INTEGER,
                     GL_UNSIGNED_INT, &got);
        fprintf(stderr, "INFO %s sampled=%u want=%u err=0x%x\n", runs[i].name,
                got, (unsigned)STENCIL_VALUE, glGetError());
        result(runs[i].name, got == STENCIL_VALUE);
        glDeleteFramebuffers(1, &fbo);
        glDeleteTextures(1, &tex);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
