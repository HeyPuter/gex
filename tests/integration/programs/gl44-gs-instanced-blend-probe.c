/* Does an instanced draw through an emulated stage rasterize its output once
 * per instance, or once per instance and again for every other instance?
 *
 * Both the geometry and the tessellation stages are emulated as compute
 * passes writing a record buffer, and instancing is unrolled into the
 * capture: the index list the rasterizing draw is handed already covers every
 * instance. That draw used to inherit instance_count from the application's,
 * so each instance's geometry was drawn `instances` times -- an
 * instances-fold overdraw.
 *
 * Nothing detected it for as long as it existed, and the reason is worth
 * keeping: the extra copies land exactly on top of the originals, and opaque
 * geometry in the same place is indistinguishable from one copy. The geometry
 * topology probe's own `instanced` case passes either way. Additive blending
 * is what separates them -- each draw adds 0.25, so the pixel *counts* the
 * draws -- which is why this probe exists rather than an extra case there.
 *
 *   `gs_instances_N`   one point through a points-in geometry shader that
 *                      emits a full-viewport triangle, drawn with N
 *                      instances. The pixel must read 0.25*N, not 0.25*N*N.
 *   `tes_instances_N`  the same shape through a tessellation evaluation
 *                      shader, which has the identical unrolling and had the
 *                      identical defect.
 *
 * The tolerance is +-3/255 and it is not slack for a driver: blending eight
 * bits at a time accumulates its own rounding, and llvmpipe reads 192 where
 * exact arithmetic gives 191.25. An overdraw is a factor, not a rounding, so
 * nothing this is meant to catch hides inside it.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <stdlib.h>

#define SIDE 64
#define STEP 0.25f

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 400 core\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";

/* One input point becomes one triangle covering the whole viewport. */
static const char *gs_src =
    "#version 400 core\n"
    "layout(points) in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "void main() {\n"
    "  gl_Position = vec4(-1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "  gl_Position = vec4( 3.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "  gl_Position = vec4(-1.0,  3.0, 0.0, 1.0); EmitVertex();\n"
    "  EndPrimitive();\n"
    "}\n";

/* One patch vertex in, one triangle out, so the tessellation path draws the
 * same figure the geometry path does. */
static const char *tcs_src =
    "#version 400 core\n"
    "layout(vertices = 1) out;\n"
    "void main() {\n"
    "  gl_TessLevelOuter[0] = 1.0;\n"
    "  gl_TessLevelOuter[1] = 1.0;\n"
    "  gl_TessLevelOuter[2] = 1.0;\n"
    "  gl_TessLevelInner[0] = 1.0;\n"
    "  gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;\n"
    "}\n";
static const char *tes_src =
    "#version 400 core\n"
    "layout(triangles, equal_spacing) in;\n"
    "void main() {\n"
    "  vec2 corner = gl_TessCoord.x * vec2(-1.0, -1.0)\n"
    "              + gl_TessCoord.y * vec2( 3.0, -1.0)\n"
    "              + gl_TessCoord.z * vec2(-1.0,  3.0);\n"
    "  gl_Position = vec4(corner, 0.0, 1.0);\n"
    "}\n";

static const char *fs_src =
    "#version 400 core\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(0.25, 0.0, 0.0, 1.0); }\n";

struct part { GLenum stage; const char *src; const char *what; };

static GLuint build(const struct part *parts, unsigned count) {
    GLuint prog = glCreateProgram();
    for (unsigned i = 0; i < count; ++i) {
        GLuint sh = glCreateShader(parts[i].stage);
        if (!sh) { glDeleteProgram(prog); return 0; }
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

/* One draw of `instances` instances, read back at the centre. */
static void measure(const char *tag, GLuint prog, GLenum mode, int instances) {
    char name[48];
    snprintf(name, sizeof(name), "%s_instances_%d", tag, instances);
    if (!prog) { result(name, 0); return; }
    glUseProgram(prog);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArraysInstanced(mode, 0, 1, instances);
    glFinish();
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    const float want = STEP * (float)instances * 255.0f;
    const int ok = abs((int)px[0] - (int)(want + 0.5f)) <= 3;
    fprintf(stderr, "INFO %s red=%u want=%d\n", name, px[0],
            (int)(want + 0.5f));
    result(name, ok);
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
    /* 4.0 for the tessellation half. */
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glEnable(GL_BLEND);
    glBlendFunc(GL_ONE, GL_ONE);

    const struct part gs_parts[3] = {
        {GL_VERTEX_SHADER, vs_src, "vs"},
        {GL_GEOMETRY_SHADER, gs_src, "gs"},
        {GL_FRAGMENT_SHADER, fs_src, "fs"},
    };
    const struct part tes_parts[4] = {
        {GL_VERTEX_SHADER, vs_src, "vs"},
        {GL_TESS_CONTROL_SHADER, tcs_src, "tcs"},
        {GL_TESS_EVALUATION_SHADER, tes_src, "tes"},
        {GL_FRAGMENT_SHADER, fs_src, "fs"},
    };
    GLuint gs_prog = build(gs_parts, 3);
    GLuint tes_prog = build(tes_parts, 4);
    if (tes_prog) glPatchParameteri(GL_PATCH_VERTICES, 1);

    /* Two and three, not one: one instance cannot show a multiplier at all,
     * and four saturates -- 0.25*4 is already 1.0, so an overdraw and the
     * right answer both read 255. Those two are where the defect is
     * visible. */
    for (int instances = 2; instances <= 3; ++instances)
        measure("gs", gs_prog, GL_POINTS, instances);
    for (int instances = 2; instances <= 3; ++instances)
        measure("tes", tes_prog, GL_PATCHES, instances);

    while (glGetError() != GL_NO_ERROR) {}
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
