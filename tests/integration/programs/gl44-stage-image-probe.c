/* An image written from a stage that is not the fragment shader.
 *
 * GL 4.2's ARB_shader_image_load_store puts image uniforms in *every* stage,
 * and this driver serves them in the fragment and compute paths only:
 * GL_MAX_VERTEX_IMAGE_UNIFORMS and GL_MAX_GEOMETRY_IMAGE_UNIFORMS are zero, so
 * the *linker* refuses such a shader before the driver sees it. Sixteen of
 * KHR-GL45.shading_language_420pack's remaining failures are this, and they
 * are invisible in a driver log -- no refusal appears anywhere, because the
 * refusal is Mesa's.
 *
 * **This probe is expected to DIFF, and the two cases fail for different
 * reasons.** That is what it is for: it says which.
 *
 *   `vertex`    platform-blocked, measured rather than assumed. With the cap
 *               raised the shader reaches Tint, which refuses it by name:
 *
 *                 storage texture with 'write' access mode cannot be used by
 *                 vertex pipeline stage
 *
 *               A vertex image that is only *read* would be expressible --
 *               Tint lowers a read-only storage image to a sampled texture --
 *               but every CTS case that binds one also stores through it.
 *   `geometry`  not blocked, just unimplemented. The geometry stage runs here
 *               as a compute pass, where a writable storage texture is
 *               allowed; it needs the image collected at shader creation and
 *               bound into the dispatch's group, which is what the compute
 *               path already does for its own.
 *
 * Both draw a single point with no fragment output that matters, so nothing
 * the fragment stage does can supply the answer; the only route from the
 * shader to the texture being read is the image store under test.
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

#define SIDE 4

static const char *fs_src =
    "#version 330 core\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(0.0, 0.0, 0.0, 1.0); }\n";

/* The store lives in whichever stage is being tested; the others pass a point
 * along untouched. */
static const char *vs_plain =
    "#version 330 core\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
static const char *vs_store =
    "#version 330 core\n"
    "#extension GL_ARB_shader_image_load_store : require\n"
    "layout(rgba8) writeonly uniform image2D img;\n"
    "void main() {\n"
    "  imageStore(img, ivec2(1, 1), vec4(0.75, 0.5, 0.25, 1.0));\n"
    "  gl_Position = vec4(0.0, 0.0, 0.0, 1.0);\n"
    "}\n";
static const char *gs_store =
    "#version 330 core\n"
    "#extension GL_ARB_shader_image_load_store : require\n"
    "layout(points) in;\n"
    "layout(points, max_vertices = 1) out;\n"
    "layout(rgba8) writeonly uniform image2D img;\n"
    "void main() {\n"
    "  imageStore(img, ivec2(1, 1), vec4(0.75, 0.5, 0.25, 1.0));\n"
    "  gl_Position = gl_in[0].gl_Position;\n"
    "  EmitVertex();\n"
    "  EndPrimitive();\n"
    "}\n";

static GLuint build(const char *vs, const char *gs) {
    struct { GLenum stage; const char *src; const char *what; } parts[3];
    unsigned n = 0;
    parts[n].stage = GL_VERTEX_SHADER;   parts[n].src = vs;
    parts[n].what = "vs"; ++n;
    if (gs) {
        parts[n].stage = GL_GEOMETRY_SHADER; parts[n].src = gs;
        parts[n].what = "gs"; ++n;
    }
    parts[n].stage = GL_FRAGMENT_SHADER; parts[n].src = fs_src;
    parts[n].what = "fs"; ++n;

    GLuint prog = glCreateProgram();
    for (unsigned i = 0; i < n; ++i) {
        GLuint sh = glCreateShader(parts[i].stage);
        if (!sh) {
            fprintf(stderr, "INFO no %s stage (0x%04x)\n", parts[i].what,
                    (unsigned)glGetError());
            glDeleteProgram(prog);
            return 0;
        }
        glShaderSource(sh, 1, &parts[i].src, NULL);
        glCompileShader(sh);
        GLint ok = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO %s compile: %s\n", parts[i].what, log);
            glDeleteShader(sh);
            glDeleteProgram(prog);
            return 0;
        }
        glAttachShader(prog, sh);
        glDeleteShader(sh);
    }
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

/* Draw one point with the store in the named stage, then read the image's
 * texel (1,1) back. Returns the red channel, or -1 on any GL error. */
static int run(GLuint prog, const char *what) {
    if (!prog) return -1;
    GLuint tex = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, SIDE, SIDE);
    const unsigned char cleared[SIDE * SIDE * 4] = {0};
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, SIDE, SIDE, GL_RGBA,
                    GL_UNSIGNED_BYTE, cleared);
    glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_WRITE_ONLY, GL_RGBA8);
    glUseProgram(prog);
    glUniform1i(glGetUniformLocation(prog, "img"), 0);
    glDrawArrays(GL_POINTS, 0, 1);
    glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);

    unsigned char texels[SIDE * SIDE * 4] = {0};
    glGetTexImage(GL_TEXTURE_2D, 0, GL_RGBA, GL_UNSIGNED_BYTE, texels);
    const GLenum error = glGetError();
    const unsigned at = (1 * SIDE + 1) * 4;
    fprintf(stderr, "INFO %s texel=%u,%u,%u err=0x%04x\n", what, texels[at],
            texels[at + 1], texels[at + 2], (unsigned)error);
    glDeleteTextures(1, &tex);
    return error == GL_NO_ERROR ? (int)texels[at] : -1;
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

    GLint vertex_images = 0, geometry_images = 0;
    glGetIntegerv(GL_MAX_VERTEX_IMAGE_UNIFORMS, &vertex_images);
    glGetIntegerv(GL_MAX_GEOMETRY_IMAGE_UNIFORMS, &geometry_images);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO version=%s vertex_images=%d geometry_images=%d\n",
            glGetString(GL_VERSION), vertex_images, geometry_images);

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* 0.75 in an 8-bit unorm is 191. */
    const int from_vertex = run(build(vs_store, NULL), "vertex");
    result("vertex", from_vertex >= 187 && from_vertex <= 195);

    const int from_geometry = run(build(vs_plain, gs_store), "geometry");
    result("geometry", from_geometry >= 187 && from_geometry <= 195);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
