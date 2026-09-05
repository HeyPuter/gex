/* An image written from a stage that is not the fragment shader.
 *
 * GL 4.2's ARB_shader_image_load_store puts image uniforms in *every* stage.
 * This driver served the fragment and compute paths only, reporting
 * GL_MAX_VERTEX_IMAGE_UNIFORMS and GL_MAX_GEOMETRY_IMAGE_UNIFORMS as zero, so
 * the *linker* refused such a shader before the driver saw it -- invisibly, no
 * refusal anywhere in a driver log, because the refusal was Mesa's.
 *
 * Every stage is served now, and this probe is expected to match its
 * reference. The geometry stage and both tessellation stages run as compute
 * passes, where WebGPU allows a writable storage texture, and needed only the
 * collection and the binding. The vertex stage needed more, because Tint
 * refuses a write-access storage texture there by name -- it rides the route
 * this driver already had for vertex storage *writes*: the shader is
 * dispatched as compute for its side effects and rasterises from a copy with
 * the stores stripped and the images removed. See the handoff document's
 * "Images in every stage".
 *
 *   `vertex`    served through the compute dispatch. Tint's refusal is real
 *               and still there --
 *
 *                 storage texture with 'write' access mode cannot be used by
 *                 vertex pipeline stage
 *
 *               -- so what rasterises must not declare the image at all; it
 *               is the dispatch that stores. This case was recorded here as
 *               platform-blocked for a while, which it was not: the wall was
 *               Tint's, and the driver already owned a way round it.
 *   `geometry`  served, and this case passes. A store of (0.75, 0.5, 0.25)
 *               reads back as 191,128,64. A *packed* geometry image -- one
 *               whose format WebGPU has no storage texture for -- is still
 *               refused by name: it needs a buffer copied into and out of the
 *               texture around the pass, and this path has nowhere to do it.
 *   `tess_control`, `tess_eval`
 *               served as well, and for the same reason. A patch of one
 *               vertex through isolines is the least that reaches both.
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

/* A tessellated patch of one vertex, which is the least that reaches both
 * tessellation stages. isolines keeps the levels to two numbers. */
static const char *tcs_plain =
    "#version 400 core\n"
    "layout(vertices = 1) out;\n"
    "void main() {\n"
    "  gl_TessLevelOuter[0] = 1.0; gl_TessLevelOuter[1] = 1.0;\n"
    "  gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;\n"
    "}\n";
static const char *tcs_store =
    "#version 400 core\n"
    "#extension GL_ARB_shader_image_load_store : require\n"
    "layout(vertices = 1) out;\n"
    "layout(rgba8) writeonly uniform image2D img;\n"
    "void main() {\n"
    "  imageStore(img, ivec2(1, 1), vec4(0.75, 0.5, 0.25, 1.0));\n"
    "  gl_TessLevelOuter[0] = 1.0; gl_TessLevelOuter[1] = 1.0;\n"
    "  gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;\n"
    "}\n";
static const char *tes_plain =
    "#version 400 core\n"
    "layout(isolines, equal_spacing) in;\n"
    "void main() { gl_Position = gl_in[0].gl_Position; }\n";
static const char *tes_store =
    "#version 400 core\n"
    "#extension GL_ARB_shader_image_load_store : require\n"
    "layout(isolines, equal_spacing) in;\n"
    "layout(rgba8) writeonly uniform image2D img;\n"
    "void main() {\n"
    "  imageStore(img, ivec2(1, 1), vec4(0.75, 0.5, 0.25, 1.0));\n"
    "  gl_Position = gl_in[0].gl_Position;\n"
    "}\n";

static GLuint build(const char *vs, const char *tcs, const char *tes,
                    const char *gs) {
    struct { GLenum stage; const char *src; const char *what; } parts[5];
    unsigned n = 0;
    parts[n].stage = GL_VERTEX_SHADER;   parts[n].src = vs;
    parts[n].what = "vs"; ++n;
    if (tcs) {
        parts[n].stage = GL_TESS_CONTROL_SHADER; parts[n].src = tcs;
        parts[n].what = "tcs"; ++n;
    }
    if (tes) {
        parts[n].stage = GL_TESS_EVALUATION_SHADER; parts[n].src = tes;
        parts[n].what = "tes"; ++n;
    }
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
static int run(GLuint prog, const char *what, int patches) {
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
    if (patches) {
        glPatchParameteri(GL_PATCH_VERTICES, 1);
        glDrawArrays(GL_PATCHES, 0, 1);
    } else {
        glDrawArrays(GL_POINTS, 0, 1);
    }
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
        /* 4.0 at least: the tessellation cases declare #version 400. */
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 1, EGL_NONE,
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
    const int from_vertex = run(build(vs_store, NULL, NULL, NULL), "vertex", 0);
    result("vertex", from_vertex >= 187 && from_vertex <= 195);

    const int from_geometry =
        run(build(vs_plain, NULL, NULL, gs_store), "geometry", 0);
    result("geometry", from_geometry >= 187 && from_geometry <= 195);

    const int from_tcs =
        run(build(vs_plain, tcs_store, tes_plain, NULL), "tess_control", 1);
    result("tess_control", from_tcs >= 187 && from_tcs <= 195);

    const int from_tes =
        run(build(vs_plain, tcs_plain, tes_store, NULL), "tess_eval", 1);
    result("tess_eval", from_tes >= 187 && from_tes <= 195);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
