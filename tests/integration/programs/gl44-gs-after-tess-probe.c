/* A geometry shader *after* tessellation, and a vertex shader that samples.
 *
 * Both stages run here as compute passes, and until now they could not be
 * chained: a patch draw with a geometry shader bound was declined by name.
 * That refusal was not the only thing in the way, and lifting it showed the
 * shape this project keeps meeting -- one refusal hiding the next. Behind it
 * was a second: the tessellated draw runs the *vertex* shader as a compute
 * pass to capture its varyings, and that capture declined any vertex shader
 * that sampled a texture, because the split had numbered its texture and
 * sampler into descriptor set 3 and a dispatch has only group zero.
 *
 * Both cases here are ordinary GL that no application would think twice
 * about, and every case in KHR-GL45.shading_language_420pack is built this
 * way -- vertex, control, evaluation, geometry and fragment, with a sampler
 * in each -- which is why that whole family of 329 failed on this.
 *
 *   `gs_after_tess`  the geometry shader shrinks each tessellated triangle
 *                    towards its centroid, so a pixel near the patch corner
 *                    is covered without the geometry stage and empty with it.
 *                    A driver that dropped the geometry stage and rasterized
 *                    the tessellated mesh directly fails this; one that drew
 *                    nothing fails the centre check beside it.
 *   `vs_samples`     the colour is fetched by the *vertex* shader from a
 *                    texture and carried through all four later stages, so a
 *                    capture pass that could not sample cannot produce it.
 *
 * The shaders declare `#version 330 core` plus the extension by name, as the
 * other tessellation probes do and for the same reason: Mesa clamps the GLSL
 * version to the granted GL version.
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

static int have_extension(const char *want) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    for (GLint i = 0; i < count; ++i) {
        const GLubyte *one = glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (one && strcmp((const char *)one, want) == 0) return 1;
    }
    return 0;
}

#define SIDE 64

/* The patch covers the whole viewport. The vertex shader samples its colour
 * from a 1x1 texture rather than computing it, which is the second case. */
static const char *vs_src =
    "#version 330 core\n"
    "in vec2 pos;\n"
    "uniform sampler2D src;\n"
    "out vec2 vpos;\n"
    "out vec3 vtint;\n"
    "void main() {\n"
    "  vpos = pos;\n"
    "  vtint = texture(src, vec2(0.5)).rgb;\n"
    "}\n";
static const char *tcs_src =
    "#version 330 core\n"
    "#extension GL_ARB_tessellation_shader : require\n"
    "layout(vertices = 4) out;\n"
    "in vec2 vpos[];\n"
    "in vec3 vtint[];\n"
    "out vec2 cpos[];\n"
    "out vec3 ctint[];\n"
    "void main() {\n"
    "  cpos[gl_InvocationID] = vpos[gl_InvocationID];\n"
    "  ctint[gl_InvocationID] = vtint[gl_InvocationID];\n"
    "  gl_TessLevelOuter[0] = 1.0; gl_TessLevelOuter[1] = 1.0;\n"
    "  gl_TessLevelOuter[2] = 1.0; gl_TessLevelOuter[3] = 1.0;\n"
    "  gl_TessLevelInner[0] = 1.0; gl_TessLevelInner[1] = 1.0;\n"
    "}\n";
static const char *tes_src =
    "#version 330 core\n"
    "#extension GL_ARB_tessellation_shader : require\n"
    "layout(quads, equal_spacing, ccw) in;\n"
    "in vec2 cpos[];\n"
    "in vec3 ctint[];\n"
    "out vec2 epos;\n"
    "out vec3 etint;\n"
    "void main() {\n"
    "  vec2 a = mix(cpos[0], cpos[1], gl_TessCoord.x);\n"
    "  vec2 b = mix(cpos[3], cpos[2], gl_TessCoord.x);\n"
    "  epos = mix(a, b, gl_TessCoord.y);\n"
    "  etint = ctint[0];\n"
    "  gl_Position = vec4(epos, 0.0, 1.0);\n"
    "}\n";
/* Every triangle is scaled by half about the origin, so the mesh covers the
 * middle of the viewport and leaves the corners bare. Scaling about the
 * *origin* rather than about each triangle's own centroid matters: a quad
 * domain is split into two triangles along a diagonal through the centre, and
 * pulling each towards its own centroid moves both off that diagonal -- so the
 * centre pixel ends up uncovered and the probe measures the split rather than
 * the geometry stage. This shape does not care how the quad was divided. */
static const char *gs_src =
    "#version 330 core\n"
    "layout(triangles) in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "in vec3 etint[];\n"
    "out vec3 gtint;\n"
    "void main() {\n"
    "  for (int i = 0; i < 3; ++i) {\n"
    "    gtint = etint[i];\n"
    "    gl_Position = vec4(gl_in[i].gl_Position.xy * 0.5,\n"
    "                       gl_in[i].gl_Position.zw);\n"
    "    EmitVertex();\n"
    "  }\n"
    "  EndPrimitive();\n"
    "}\n";
static const char *fs_src =
    "#version 330 core\n"
    "in vec3 gtint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(gtint, 1.0); }\n";

static GLuint build(void) {
    struct { GLenum stage; const char *src; const char *what; } parts[] = {
        {GL_VERTEX_SHADER, vs_src, "vs"},
        {GL_TESS_CONTROL_SHADER, tcs_src, "tcs"},
        {GL_TESS_EVALUATION_SHADER, tes_src, "tes"},
        {GL_GEOMETRY_SHADER, gs_src, "gs"},
        {GL_FRAGMENT_SHADER, fs_src, "fs"},
    };
    GLuint prog = glCreateProgram();
    for (unsigned i = 0; i < sizeof(parts) / sizeof(parts[0]); ++i) {
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
    glBindAttribLocation(prog, 0, "pos");
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

    fprintf(stderr, "INFO version=%s tess=%d\n", glGetString(GL_VERSION),
            have_extension("GL_ARB_tessellation_shader"));

    GLuint colour = 0, fbo = 0, vao = 0, buffer = 0, tex = 0;
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

    /* The colour the vertex shader fetches: a distinctive orange, so a
     * default-coloured pixel cannot be mistaken for a sampled one. */
    const unsigned char texel[4] = {230, 120, 30, 255};
    glGenTextures(1, &tex);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, texel);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    /* One quad patch over the whole clip volume, wound counter-clockwise. */
    const float corners[8] = {
        -1.0f, -1.0f,   1.0f, -1.0f,   1.0f, 1.0f,   -1.0f, 1.0f,
    };
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(corners), corners, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 2 * sizeof(float), NULL);

    GLuint prog = build();
    if (!prog) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    glUniform1i(glGetUniformLocation(prog, "src"), 0);
    glPatchParameteri(GL_PATCH_VERTICES, 4);
    glDrawArrays(GL_PATCHES, 0, 4);

    unsigned char centre[4] = {0}, corner[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, centre);
    /* Two pixels in from the corner: inside the untouched tessellated mesh,
     * outside the halved one. A driver that dropped the geometry stage and
     * rasterized the evaluation shader's output covers this pixel. */
    glReadPixels(2, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, corner);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO centre=%u,%u,%u corner=%u,%u,%u err=0x%04x\n",
            centre[0], centre[1], centre[2], corner[0], corner[1], corner[2],
            (unsigned)error);
    result("gs_after_tess", error == GL_NO_ERROR && centre[0] > 128 &&
                            corner[0] < 32 && corner[1] < 32 && corner[2] < 32);
    /* The orange came from the texture, through five stages. */
    result("vs_samples", error == GL_NO_ERROR && centre[0] > 200 &&
                         centre[1] > 90 && centre[1] < 150 && centre[2] < 70);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
