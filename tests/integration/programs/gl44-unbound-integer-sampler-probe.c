/* An integer sampler at a texture unit with nothing bound.
 *
 * A shader declaring `usampler2D` has no base module at all: integer sampling
 * is resolved against the *bound* view's format, so the module is deferred to
 * a draw-time variant (see deferred_integer_sampling). The variant is built
 * only when the fragment module decides it needs one -- and that decision
 * used to set its integer-sampling reason **inside** a
 * `uses_sampler && view && sampler` guard, so a unit with nothing bound gave
 * no reason, no variant was built, and `fs->module` was returned: NULL. The
 * draw then vanished at `if (!fragment.module)` with no message.
 *
 * GL does not make that undefined-but-fatal. Reading an incomplete texture
 * yields (0,0,0,1) and the draw still happens, so the shape to check is that
 * the *other* colour channels a shader writes come through -- a draw that
 * disappeared writes nothing at all, which is what separates the two.
 *
 *   `unbound_usampler`   one usampler2D declared and read, no texture bound.
 *                        The shader writes a constant into green, so green
 *                        proves the draw happened at all and red reports
 *                        whatever the incomplete fetch gave.
 *   `unbound_isampler`   the signed twin, which takes a different lowering
 *                        path in the driver's texture variant.
 *   `bound_usampler`     the same shader with a real R32UI texture bound, so
 *                        the case above cannot pass by drawing nothing while
 *                        this one also fails.
 *   `unbound_float_target`
 *                        the same unbound usampler writing an ordinary vec4
 *                        to an RGBA8 target. This is the case that actually
 *                        reaches the gap: an *integer* colour buffer makes
 *                        the driver retype its outputs, which is a variant
 *                        reason of its own and forces the variant the three
 *                        cases above then get for free. With a float target
 *                        and no inputs there is no other reason at all.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

#define SIDE 16

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 330 core\n"
    "void main() {\n"
    /* A full-viewport triangle from gl_VertexID, so the probe needs no
       vertex buffer and no attribute state can affect the outcome. */
    "  vec2 p = vec2((gl_VertexID == 1) ? 3.0 : -1.0,\n"
    "                (gl_VertexID == 2) ? 3.0 : -1.0);\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "}\n";

/* Green is the constant that proves the draw ran; red carries the fetch. */
static const char *fs_u_src =
    "#version 330 core\n"
    "uniform usampler2D tex;\n"
    "out uvec4 colour;\n"
    "void main() {\n"
    "  uvec4 t = texelFetch(tex, ivec2(0), 0);\n"
    "  colour = uvec4(t.r, 7u, 0u, 1u);\n"
    "}\n";
/* An integer sampler read into a *float* output, so nothing about the colour
   buffer forces a variant. Green is 7/255 for the same reason as above. */
static const char *fs_u_float_src =
    "#version 330 core\n"
    "uniform usampler2D tex;\n"
    "out vec4 colour;\n"
    "void main() {\n"
    "  uvec4 t = texelFetch(tex, ivec2(0), 0);\n"
    "  colour = vec4(float(t.r) / 255.0, 7.0 / 255.0, 0.0, 1.0);\n"
    "}\n";
static const char *fs_i_src =
    "#version 330 core\n"
    "uniform isampler2D tex;\n"
    "out uvec4 colour;\n"
    "void main() {\n"
    "  ivec4 t = texelFetch(tex, ivec2(0), 0);\n"
    "  colour = uvec4(uint(t.r), 7u, 0u, 1u);\n"
    "}\n";

static GLuint build(const char *fs_src) {
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
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[600] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(prog);
        return 0;
    }
    glDeleteShader(vs);
    glDeleteShader(fs);
    return prog;
}

/* The green channel is the whole test: 7 means the fragment shader ran. */
static void run(const char *name, GLuint prog) {
    if (!prog) { result(name, 0); return; }
    glUseProgram(prog);
    const GLuint zero[4] = {0, 0, 0, 0};
    glClearBufferuiv(GL_COLOR, 0, zero);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    GLuint px[4] = {0, 0, 0, 0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA_INTEGER, GL_UNSIGNED_INT, px);
    fprintf(stderr, "INFO %s rgba=%u,%u,%u,%u err=0x%04x\n", name,
            px[0], px[1], px[2], px[3], (unsigned)glGetError());
    result(name, px[1] == 7u);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (d == EGL_NO_DISPLAY || !eglInitialize(d, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint ca[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                         EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE};
    EGLConfig cfg = NULL; EGLint n = 0;
    if (!eglChooseConfig(d, ca, &cfg, 1, &n) || n < 1) return 2;
    const EGLint cxa[] = {EGL_CONTEXT_MAJOR_VERSION, 3,
                          EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE};
    const EGLint sa[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext cx = eglCreateContext(d, cfg, EGL_NO_CONTEXT, cxa);
    EGLSurface sf = eglCreatePbufferSurface(d, cfg, sa);
    if (cx == EGL_NO_CONTEXT || sf == EGL_NO_SURFACE ||
        !eglMakeCurrent(d, sf, sf, cx)) return 2;

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32UI, SIDE, SIDE, 0,
                 GL_RGBA_INTEGER, GL_UNSIGNED_INT, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    /* Nothing bound to unit 0 for the first two. */
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, 0);
    run("unbound_usampler", build(fs_u_src));
    run("unbound_isampler", build(fs_i_src));

    /* And with a real integer texture, so a driver that draws nothing at all
       cannot pass the two above by accident. */
    GLuint src = 0;
    glGenTextures(1, &src);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, src);
    const GLuint texel = 5;
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R32UI, 1, 1, 0, GL_RED_INTEGER,
                 GL_UNSIGNED_INT, &texel);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    run("bound_usampler", build(fs_u_src));

    /* And the float-target case, on a framebuffer of its own. */
    {
        GLuint rgba = 0, fbo8 = 0;
        glGenTextures(1, &rgba);
        glBindTexture(GL_TEXTURE_2D, rgba);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                     GL_UNSIGNED_BYTE, NULL);
        glGenFramebuffers(1, &fbo8);
        glBindFramebuffer(GL_FRAMEBUFFER, fbo8);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                               GL_TEXTURE_2D, rgba, 0);
        glViewport(0, 0, SIDE, SIDE);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, 0);
        GLuint prog = build(fs_u_float_src);
        int ok = 0;
        if (prog) {
            glUseProgram(prog);
            glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            unsigned char px[4] = {0, 0, 0, 0};
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA,
                         GL_UNSIGNED_BYTE, px);
            fprintf(stderr, "INFO unbound_float_target rgba=%u,%u,%u,%u "
                    "err=0x%04x\n", px[0], px[1], px[2], px[3],
                    (unsigned)glGetError());
            ok = px[1] == 7u;
        }
        result("unbound_float_target", ok);
    }

    while (glGetError() != GL_NO_ERROR) {}
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
