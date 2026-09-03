/* ARB_tessellation_shader -- the one thing GL 4.0 still wants.
 *
 * `tests/gl-version-ladder.sh` says GL 4.0 is missing exactly this and
 * nothing else, and Mesa's ladder is strict, so the version cannot rise past
 * 3.3 until it is answered. That matters beyond the number: Mesa clamps the
 * usable GLSL version to the *granted* GL version
 * (`_mesa_compute_version`), so no shader can declare above `#version 330`
 * while the context is 3.3 -- and modern Blender and Minecraft 26.2 both want
 * more. One extension gates all of it.
 *
 * WebGPU has no tessellation stage, so this has to be emulated, and the shape
 * of an answer is the geometry shader's: run the stages as compute and
 * rasterize the result. What makes tessellation harder than the geometry stage
 * is the *fixed-function tessellator* between the two shader stages -- the
 * thing that turns two levels and a domain into a mesh of (u,v,w) coordinates
 * and a topology. Its output depends on levels the control shader computes on
 * the GPU.
 *
 * So the cases below are chosen to pin down the tessellator, not just to check
 * that something drew. A tessellation-evaluation shader that maps the domain
 * linearly draws the *same picture* at every level, which is why "it renders"
 * is not evidence: each case here interpolates a **non-linear** per-vertex
 * value across the patch, so the centre pixel reads the mesh density directly.
 * At level 1 a quad is two triangles and its corners carry f=0, so the centre
 * is 0; at level 2 there is a vertex at the middle carrying f=1 and the centre
 * jumps. A driver that ignores the levels, or gets the spacing rule wrong, or
 * triangulates a different way, reads back a different number -- and llvmpipe
 * supplies what the number should be, so none of them have to be worked out
 * here.
 *
 * The shaders declare `#version 330 core` plus the extension by name rather
 * than `#version 400`, because the GLSL clamp above means 400 is not available
 * in the context this driver grants. ARB_tessellation_shader is written
 * against GLSL 1.50, so this is the spelling that works on both drivers.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 64

static int have_extension(const char *want)
{
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

/* The control shader is the same in every case bar the levels it writes, and
 * the evaluation shader varies by domain and spacing, so both are assembled
 * from a format string. */
static GLuint build(const char *tcs_body, const char *tes_layout,
                    const char *tes_body, const char *name)
{
    static const char *vs_src =
        "#version 330 core\n"
        "in vec2 pos;\n"
        "out vec2 vpos;\n"
        "void main() { vpos = pos; }\n";
    static const char *fs_src =
        "#version 330 core\n"
        "in float f;\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(f, 0.0, 0.0, 1.0); }\n";
    char tcs_src[1024], tes_src[1024];
    snprintf(tcs_src, sizeof(tcs_src),
             "#version 330 core\n"
             "#extension GL_ARB_tessellation_shader : require\n"
             "layout(vertices = 4) out;\n"
             "in vec2 vpos[];\n"
             "out vec2 cpos[];\n"
             "uniform float level;\n"
             "void main() {\n"
             "  cpos[gl_InvocationID] = vpos[gl_InvocationID];\n"
             "  %s\n"
             "}\n", tcs_body);
    snprintf(tes_src, sizeof(tes_src),
             "#version 330 core\n"
             "#extension GL_ARB_tessellation_shader : require\n"
             "%s\n"
             "in vec2 cpos[];\n"
             "out float f;\n"
             "void main() {\n"
             "  %s\n"
             "}\n", tes_layout, tes_body);

    struct { GLenum stage; const char *src; const char *what; } parts[] = {
        {GL_VERTEX_SHADER, vs_src, "vs"},
        {GL_TESS_CONTROL_SHADER, tcs_src, "tcs"},
        {GL_TESS_EVALUATION_SHADER, tes_src, "tes"},
        {GL_FRAGMENT_SHADER, fs_src, "fs"},
    };
    GLuint prog = glCreateProgram();
    for (unsigned i = 0; i < sizeof(parts) / sizeof(parts[0]); ++i) {
        GLuint sh = glCreateShader(parts[i].stage);
        if (!sh) {
            fprintf(stderr, "INFO %s: no %s stage (0x%04x)\n", name,
                    parts[i].what, (unsigned)glGetError());
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
            fprintf(stderr, "INFO %s %s compile: %s\n", name, parts[i].what,
                    log);
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
        fprintf(stderr, "INFO %s link: %s\n", name, log);
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

    const int have = have_extension("GL_ARB_tessellation_shader");
    GLint max_level = 0, max_patch = 0;
    glGetIntegerv(GL_MAX_TESS_GEN_LEVEL, &max_level);
    glGetIntegerv(GL_MAX_PATCH_VERTICES, &max_patch);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO version=%s tessellation=%d max_gen_level=%d "
            "max_patch_vertices=%d\n", glGetString(GL_VERSION), have,
            max_level, max_patch);

    GLuint colour = 0, fbo = 0, vao = 0, vbo = 0;
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

    /* One patch: the four corners of clip space, in the winding a quad domain
     * expects (u along 0->1, v along 0->3). */
    static const float patch[8] = {
        -1.0f, -1.0f,   1.0f, -1.0f,   1.0f,  1.0f,  -1.0f,  1.0f,
    };
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof(patch), patch, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    /* Bilinear position from the four control points, and a value that peaks
     * in the middle of the patch: f = 4*u*(1-u)*4*v*(1-v). Every corner
     * carries 0, so at level 1 the whole patch interpolates to 0 and any
     * finer level lifts the centre. */
    static const char *quad_pos =
        "  vec2 a = mix(cpos[0], cpos[1], gl_TessCoord.x);\n"
        "  vec2 b = mix(cpos[3], cpos[2], gl_TessCoord.x);\n"
        "  vec2 p = mix(a, b, gl_TessCoord.y);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "  float u = gl_TessCoord.x, v = gl_TessCoord.y;\n"
        "  f = 16.0 * u * (1.0 - u) * v * (1.0 - v);\n";
    /* The triangle domain's barycentric coordinates, over the first three
     * control points, with a value peaking at the centroid. */
    static const char *tri_pos =
        "  vec2 p = gl_TessCoord.x * cpos[0] + gl_TessCoord.y * cpos[1] +\n"
        "           gl_TessCoord.z * cpos[2];\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "  f = 27.0 * gl_TessCoord.x * gl_TessCoord.y * gl_TessCoord.z;\n";

    static const char *set_all =
        "  gl_TessLevelOuter[0] = level;\n"
        "  gl_TessLevelOuter[1] = level;\n"
        "  gl_TessLevelOuter[2] = level;\n"
        "  gl_TessLevelOuter[3] = level;\n"
        "  gl_TessLevelInner[0] = level;\n"
        "  gl_TessLevelInner[1] = level;\n";

    struct {
        const char *name;
        const char *tcs_body;
        const char *tes_layout;
        const char *tes_body;
        float level;
        /* What llvmpipe reads back at the two sample points. The GL
         * specification pins the tessellator's output exactly for a given
         * domain, spacing and level, so these are not one implementation's
         * private answer -- and llvmpipe reaches them through the same
         * src/gallium/auxiliary/tessellator this driver does. */
        unsigned want_a, want_b;
        /* Whether each sample point should have been covered at all. At level
         * 1 the quad is two triangles whose corners all carry f = 0, so it
         * draws a black patch -- indistinguishable from drawing nothing if
         * only the red channel is read. The clear is green and the shader
         * writes no green, so a covered pixel has green 0 and an untouched
         * one has 255, which separates "correctly black" from "absent". */
        int drawn_a, drawn_b;
    } tests[] = {
        /* The density ladder: same shaders, three levels, three answers. */
        {"quad-l1", set_all, "layout(quads, equal_spacing) in;", quad_pos, 1.0f, 0, 0, 1, 1},
        {"quad-l2", set_all, "layout(quads, equal_spacing) in;", quad_pos, 2.0f, 124, 187, 1, 1},
        {"quad-l8", set_all, "layout(quads, equal_spacing) in;", quad_pos, 8.0f, 142, 235, 1, 1},
        /* The triangle domain, whose coordinates are barycentric and whose
         * inner ring is generated by a different rule. */
        {"tri-l2", set_all, "layout(triangles, equal_spacing) in;", tri_pos,
         2.0f, 185, 96, 1, 1},
        {"tri-l6", set_all, "layout(triangles, equal_spacing) in;", tri_pos,
         6.0f, 205, 152, 1, 1},
        /* Spacing. A fractional level is where the three rules diverge: equal
         * spacing rounds up and spreads evenly, odd and even keep a
         * differently-sized pair of end segments. */
        {"quad-frac-odd", set_all,
         "layout(quads, fractional_odd_spacing) in;", quad_pos, 5.3f, 137, 223, 1, 1},
        {"quad-frac-even", set_all,
         "layout(quads, fractional_even_spacing) in;", quad_pos, 5.3f, 141, 227, 1, 1},
        /* Outer levels that differ from each other and from the inner one,
         * which is what a level-of-detail patch actually produces -- and the
         * case a driver that reads one level for all six passes wrongly. */
        {"quad-mixed",
         "  gl_TessLevelOuter[0] = 2.0;\n"
         "  gl_TessLevelOuter[1] = 4.0;\n"
         "  gl_TessLevelOuter[2] = 8.0;\n"
         "  gl_TessLevelOuter[3] = 3.0;\n"
         "  gl_TessLevelInner[0] = 5.0;\n"
         "  gl_TessLevelInner[1] = 6.0;\n",
         "layout(quads, equal_spacing) in;", quad_pos, 0.0f, 136, 231, 1, 1},
        /* A level computed from a uniform rather than written as a constant,
         * which is how every real tessellation shader works and which no
         * constant-folding shortcut can serve. */
        {"quad-uniform-level",
         "  float l = level * 2.0 + 1.0;\n"
         "  gl_TessLevelOuter[0] = l;\n"
         "  gl_TessLevelOuter[1] = l;\n"
         "  gl_TessLevelOuter[2] = l;\n"
         "  gl_TessLevelOuter[3] = l;\n"
         "  gl_TessLevelInner[0] = l;\n"
         "  gl_TessLevelInner[1] = l;\n",
         "layout(quads, equal_spacing) in;", quad_pos, 3.0f, 141, 227, 1, 1},
        /* Culling the patch entirely: an outer level of zero discards it, and
         * that is how a tessellation shader rejects off-screen geometry. */
        {"quad-culled",
         "  gl_TessLevelOuter[0] = -1.0;\n"
         "  gl_TessLevelOuter[1] = 4.0;\n"
         "  gl_TessLevelOuter[2] = 4.0;\n"
         "  gl_TessLevelOuter[3] = 4.0;\n"
         "  gl_TessLevelInner[0] = 4.0;\n"
         "  gl_TessLevelInner[1] = 4.0;\n",
         "layout(quads, equal_spacing) in;", quad_pos, 0.0f, 0, 0, 0, 0},
        /* Clockwise winding, which flips which side of every generated
         * triangle faces the viewer -- with culling on, the wrong order draws
         * nothing. */
        {"quad-cw", set_all,
         "layout(quads, equal_spacing, cw) in;", quad_pos, 4.0f, 0, 0, 0, 0},
        /* The control for it: the same draw, the same culling, the default
         * winding. Without this pair a driver that ignores the winding and a
         * driver that inverts it look identical -- one case can only say
         * "wrong", not which way. */
        {"quad-ccw", set_all,
         "layout(quads, equal_spacing, ccw) in;", quad_pos, 4.0f,
         140, 220, 1, 1},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        unsigned char centre[4] = {0}, off[4] = {0};
        GLuint prog = 0;
        if (have) {
            prog = build(tests[t].tcs_body, tests[t].tes_layout,
                         tests[t].tes_body, tests[t].name);
        }
        glClearColor(0, 1, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            const GLint loc = glGetUniformLocation(prog, "level");
            if (loc >= 0) glUniform1f(loc, tests[t].level);
            glPatchParameteri(GL_PATCH_VERTICES, 4);
            /* Culling on for the winding case and off otherwise: `cw` is only
             * observable through it. */
            if (strncmp(tests[t].name, "quad-c", 6) == 0) {
                glEnable(GL_CULL_FACE);
                glCullFace(GL_BACK);
                glFrontFace(GL_CCW);
            } else {
                glDisable(GL_CULL_FACE);
            }
            glDrawArrays(GL_PATCHES, 0, 4);
            glDisable(GL_CULL_FACE);
        }
        /* Two samples, both chosen to lie inside *every* domain drawn here.
         * The obvious pair -- the viewport centre and a point a quarter in --
         * was wrong: the triangle domain over three of the four control
         * points is the lower-right half of the viewport, so the centre sits
         * exactly on its hypotenuse and the quarter point is outside it
         * altogether. Both triangle cases read 0 and looked like a driver
         * with no triangle domain. */
        glReadPixels(SIDE * 3 / 4, SIDE / 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                     centre);
        glReadPixels(SIDE * 5 / 8, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                     off);
        const GLenum error = glGetError();
        fprintf(stderr,
                "INFO %s a=%u/%s b=%u/%s want=%u/%s,%u/%s err=0x%04x\n",
                tests[t].name, centre[0], centre[1] ? "clear" : "drawn",
                off[0], off[1] ? "clear" : "drawn",
                tests[t].want_a, tests[t].drawn_a ? "drawn" : "clear",
                tests[t].want_b, tests[t].drawn_b ? "drawn" : "clear",
                (unsigned)error);
        /* There is no expected value written here: the numbers depend on the
         * tessellator's exact output and llvmpipe is the reference. The INFO
         * lines above are what the comparison reads, and the case passes when
         * the program built, drew without error, and produced *something* --
         * a driver with no tessellation at all fails on the first of those. */
        result(tests[t].name,
               have && prog && error == GL_NO_ERROR &&
               (centre[1] == 0) == (tests[t].drawn_a != 0) &&
               (off[1] == 0) == (tests[t].drawn_b != 0) &&
               (!tests[t].drawn_a ||
                abs((int)centre[0] - (int)tests[t].want_a) <= 3) &&
               (!tests[t].drawn_b ||
                abs((int)off[0] - (int)tests[t].want_b) <= 3));
        if (prog) glDeleteProgram(prog);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
