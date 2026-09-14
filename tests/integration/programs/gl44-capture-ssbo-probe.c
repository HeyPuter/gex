/* A vertex shader that reads a storage buffer, drawn through every path that
 * replays the vertex stage as a compute dispatch.
 *
 * Six paths dispatch a "varying variant" -- a compute clone of the vertex
 * shader that writes its varyings to a buffer for a later pass to consume.
 * Until this probe existed, only two of them bound the application's storage
 * blocks into the dispatch's group one. The other four left the module's
 * `@group(1) @binding(32)` unmatched by the bind group layout, which is not a
 * wrong picture but an *invalid pipeline*:
 *
 *   Binding doesn't exist in [BindGroupLayoutInternal "tess vs capture g1"].
 *    - While validating that the entry-point's declaration for
 *      @group(1) @binding(32) matches ...
 *
 * and an invalid pipeline invalidates the command buffer, so every draw
 * already recorded with it is lost as well. That is the failure mode this
 * driver keeps producing and no ordinary test sees: not a crash, not a wrong
 * colour, just nothing.
 *
 * It went unnoticed for weeks because the only case that reached it --
 * KHR-GL45.shader_atomic_counter_ops_tests.AdditionSubstraction -- reported a
 * *wrong return value*, which pointed at the atomic-result replay, and the
 * binding message named no pass. Labelling the bind group layouts is what
 * turned it into one run.
 *
 * Each case names the path it covers, so a failure says which dispatch
 * dropped the bind rather than "storage buffers are broken":
 *
 *   plain          the ordinary rasterised draw. The control: a failure here
 *                  is not about the capture paths at all.
 *   geometry       through a geometry shader   -- gs_draw's vertex capture
 *   tessellation   through a tessellation pair -- tess_draw's vertex capture
 *   wide_lines     glLineWidth > 1             -- line_draw's vertex capture
 *   flat_unfilled  flat shading + GL_LINE      -- flat_unfilled_draw's
 *   cull_distance  a gl_CullDistance write   -- capture_positions, which the
 *                  cull filter runs to find which primitives survive
 *   write_through  the same shader writing a block through the geometry path,
 *                  so the read-only/writable half of the layout is exercised
 *                  too: the capture module's access qualifier and the layout
 *                  must agree or the layout is rejected the same way.
 *
 * The geometry and tessellation stages deliberately do nothing but pass the
 * vertices through. The point is the *vertex* stage's storage block surviving
 * the trip, not what the later stage computes.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok, const char *detail) {
    ++cases;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-16s %s\n", name, detail);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 32

/* The corners come out of the block, so nothing lands where it should unless
 * the read worked -- and the colour comes out of the block too, so a draw that
 * ran with a *zeroed* binding is a different picture from one that ran with
 * the right one. Both halves matter: the failure being caught here leaves the
 * draw missing entirely, but a future one that binds the wrong buffer would
 * show up as the wrong colour rather than as a pass. */
static const char *vs_read =
    "#version 430 core\n"
    "layout(std430, binding = 0) readonly buffer Corners {\n"
    "  vec4 at[];\n"
    "} corners;\n"
    "out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(corners.at[gl_VertexID].xy, 0.0, 1.0);\n"
    "  tint = corners.at[6].rgb * (corners.at.length() == 7 ? 1.0 : 0.0);\n"
    "}\n";
/* The same read plus a write, for the write_through case. A vertex-stage
 * storage write is stripped from the rasterising module and replayed, so this
 * shader takes the *other* clone -- and the two clones disagree about whether
 * the block is writable, which is exactly what a layout built from the wrong
 * one gets wrong. */
static const char *vs_read_write =
    "#version 430 core\n"
    "layout(std430, binding = 0) readonly buffer Corners {\n"
    "  vec4 at[];\n"
    "} corners;\n"
    "layout(std430, binding = 1) buffer Tally { uint hits; } tally;\n"
    "out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(corners.at[gl_VertexID].xy, 0.0, 1.0);\n"
    "  tint = corners.at[6].rgb * (corners.at.length() == 7 ? 1.0 : 0.0);\n"
    "  atomicAdd(tally.hits, 1u);\n"
    "}\n";
static const char *fs_plain =
    "#version 430 core\n"
    "in vec3 tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(tint, 1.0); }\n";
/* Flat, for the flat_unfilled case: a flat-shaded unfilled polygon is the one
 * shape whose colour cannot come from the rasteriser, so the driver replays
 * the vertex stage to get it. */
static const char *fs_flat =
    "#version 430 core\n"
    "flat in vec3 tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(tint, 1.0); }\n";
static const char *vs_read_flat =
    "#version 430 core\n"
    "layout(std430, binding = 0) readonly buffer Corners {\n"
    "  vec4 at[];\n"
    "} corners;\n"
    "flat out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(corners.at[gl_VertexID].xy, 0.0, 1.0);\n"
    "  tint = corners.at[6].rgb * (corners.at.length() == 7 ? 1.0 : 0.0);\n"
    "}\n";
static const char *gs_passthrough =
    "#version 430 core\n"
    "layout(triangles) in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "in vec3 tint[];\n"
    "out vec3 gtint;\n"
    "void main() {\n"
    "  for (int i = 0; i < 3; ++i) {\n"
    "    gl_Position = gl_in[i].gl_Position;\n"
    "    gtint = tint[i];\n"
    "    EmitVertex();\n"
    "  }\n"
    "  EndPrimitive();\n"
    "}\n";
static const char *fs_from_gs =
    "#version 430 core\n"
    "in vec3 gtint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(gtint, 1.0); }\n";
static const char *tcs_passthrough =
    "#version 430 core\n"
    "layout(vertices = 3) out;\n"
    "in vec3 tint[];\n"
    "out vec3 ttint[];\n"
    "void main() {\n"
    "  gl_out[gl_InvocationID].gl_Position =\n"
    "    gl_in[gl_InvocationID].gl_Position;\n"
    "  ttint[gl_InvocationID] = tint[gl_InvocationID];\n"
    "  gl_TessLevelOuter[0] = 1.0;\n"
    "  gl_TessLevelOuter[1] = 1.0;\n"
    "  gl_TessLevelOuter[2] = 1.0;\n"
    "  gl_TessLevelInner[0] = 1.0;\n"
    "}\n";
static const char *tes_passthrough =
    "#version 430 core\n"
    "layout(triangles, equal_spacing, ccw) in;\n"
    "in vec3 ttint[];\n"
    "out vec3 etint;\n"
    "void main() {\n"
    "  gl_Position = gl_TessCoord.x * gl_in[0].gl_Position +\n"
    "                gl_TessCoord.y * gl_in[1].gl_Position +\n"
    "                gl_TessCoord.z * gl_in[2].gl_Position;\n"
    "  etint = gl_TessCoord.x * ttint[0] + gl_TessCoord.y * ttint[1] +\n"
    "          gl_TessCoord.z * ttint[2];\n"
    "}\n";
/* gl_CullDistance sends the draw through gex_wgpu_capture_positions -- the
 * cull filter replays the vertex stage to learn which primitives survive. The
 * distance is positive everywhere, so nothing is actually culled and the
 * picture is the same triangle: the case is about the *path*, not the cull. */
static const char *vs_read_cull =
    "#version 430 core\n"
    "#extension GL_ARB_cull_distance : require\n"
    "layout(std430, binding = 0) readonly buffer Corners {\n"
    "  vec4 at[];\n"
    "} corners;\n"
    "out float gl_CullDistance[1];\n"
    "out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(corners.at[gl_VertexID].xy, 0.0, 1.0);\n"
    "  tint = corners.at[6].rgb * (corners.at.length() == 7 ? 1.0 : 0.0);\n"
    "  gl_CullDistance[0] = gl_VertexID < 3 ? 1.0 : -1.0;\n"
    "}\n";
static const char *fs_from_tes =
    "#version 430 core\n"
    "in vec3 etint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(etint, 1.0); }\n";

struct stage { GLenum type; const char *src; };

static GLuint build(const struct stage *stages, unsigned count) {
    GLuint prog = glCreateProgram();
    for (unsigned i = 0; i < count; ++i) {
        GLuint sh = glCreateShader(stages[i].type);
        glShaderSource(sh, 1, &stages[i].src, NULL);
        glCompileShader(sh);
        GLint ok = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO compile (stage 0x%x): %s\n",
                    (unsigned)stages[i].type, log);
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

static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }

/* The pixel at the centre of the covered area. The quad the block describes
 * covers the whole viewport, so the centre is lit whichever primitive mode
 * drew it -- except for the unfilled and line cases, which are checked at a
 * point the outline passes through instead. */
static void read_pixel(int x, int y, unsigned char *out) {
    glReadPixels(x, y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, out);
}

/* Lit with the tint the block carries: 0x00, 0xff, 0x80 in eight-bit terms.
 * Both halves are checked, so "the draw happened" and "the draw read the right
 * buffer" are different answers -- see zero-means-nobody-wrote. */
static int is_tint(const unsigned char *p) {
    return p[0] < 32 && p[1] > 200 && p[2] > 96 && p[2] < 160;
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
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

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

    /* Three corners of a triangle, a fourth the strip cases do not use, then
     * the tint.
     *
     * Inset from the viewport edge on purpose. The first version ran the
     * triangle off the edges, and its *outline* then lay exactly along the
     * bottom and left rows, where line rasterisation is ambiguous -- the
     * llvmpipe reference drew nothing there and the case failed on the
     * reference, which is a broken probe rather than a driver result. Inset,
     * every edge lands on an interior row that both drivers agree on. */
    const float block[] = {
        /* Triangle A, covering the lower-left. Every case draws this one. */
        -0.9f, -0.9f, 0.0f, 1.0f,
         0.9f, -0.9f, 0.0f, 1.0f,
        -0.9f,  0.9f, 0.0f, 1.0f,
        /* Triangle B, in the upper-right corner and disjoint from A. Only the
         * cull case draws it, and culls it -- so that the case can tell a lost
         * capture from a working one. With nothing culled the picture is the
         * same whether the replay ran or not, which is how the first version
         * of this case passed on a driver that lost the dispatch. */
         0.5f,  0.5f, 0.0f, 1.0f,
         0.9f,  0.5f, 0.0f, 1.0f,
         0.9f,  0.9f, 0.0f, 1.0f,
         0.0f,  1.0f, 0.5f, 1.0f,   /* the tint: black-free green, half blue */
    };
    GLuint corners = 0;
    glGenBuffers(1, &corners);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, corners);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(block), block,
                 GL_STATIC_DRAW);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, corners);

    GLuint tally = 0;
    const GLuint tally_seed = 0;
    glGenBuffers(1, &tally);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, tally);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(tally_seed), &tally_seed,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, tally);
    drain();

    unsigned char px[4];
    char detail[160];

    /* 1. plain: the control. */
    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_read},
                                  {GL_FRAGMENT_SHADER, fs_plain}};
        GLuint prog = build(s, 2);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) { glUseProgram(prog); glDrawArrays(GL_TRIANGLES, 0, 3); }
        glFinish();
        read_pixel(SIDE / 4, SIDE / 4, px);
        const GLenum err = glGetError();
        snprintf(detail, sizeof(detail), "rgb=%u,%u,%u err=0x%04x", px[0],
                 px[1], px[2], (unsigned)err);
        result("plain", prog && is_tint(px), detail);
        glDeleteProgram(prog);
        drain();
    }

    /* 2. geometry: gs_draw's vertex capture. */
    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_read},
                                  {GL_GEOMETRY_SHADER, gs_passthrough},
                                  {GL_FRAGMENT_SHADER, fs_from_gs}};
        GLuint prog = build(s, 3);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) { glUseProgram(prog); glDrawArrays(GL_TRIANGLES, 0, 3); }
        glFinish();
        read_pixel(SIDE / 4, SIDE / 4, px);
        const GLenum err = glGetError();
        snprintf(detail, sizeof(detail), "rgb=%u,%u,%u err=0x%04x", px[0],
                 px[1], px[2], (unsigned)err);
        result("geometry", prog && is_tint(px), detail);
        glDeleteProgram(prog);
        drain();
    }

    /* 3. tessellation: tess_draw's vertex capture. This is the case that was
     * actually failing, reached in the CTS only through an atomic counter. */
    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_read},
                                  {GL_TESS_CONTROL_SHADER, tcs_passthrough},
                                  {GL_TESS_EVALUATION_SHADER, tes_passthrough},
                                  {GL_FRAGMENT_SHADER, fs_from_tes}};
        GLuint prog = build(s, 4);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glPatchParameteri(GL_PATCH_VERTICES, 3);
            glDrawArrays(GL_PATCHES, 0, 3);
        }
        glFinish();
        read_pixel(SIDE / 4, SIDE / 4, px);
        const GLenum err = glGetError();
        snprintf(detail, sizeof(detail), "rgb=%u,%u,%u err=0x%04x", px[0],
                 px[1], px[2], (unsigned)err);
        result("tessellation", prog && is_tint(px), detail);
        glDeleteProgram(prog);
        drain();
    }

    /* 4. wide_lines: line_draw's vertex capture. A line wider than one pixel
     * is drawn as quads built by a compute pass, and that pass replays the
     * vertex shader. Sampled on the diagonal edge the first two corners span,
     * which passes through the viewport's lower-left. */
    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_read},
                                  {GL_FRAGMENT_SHADER, fs_plain}};
        GLuint prog = build(s, 2);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glLineWidth(5.0f);
            glDrawArrays(GL_LINES, 0, 2);
            glLineWidth(1.0f);
        }
        glFinish();
        /* The line runs along y = -0.9 in clip space, which the reference
         * rasterises into row 1; five pixels wide, so row 1 is well inside
         * it. The row was read off a dump of the reference rather than
         * derived, because a probe whose sample point is one row out fails on
         * the reference and says nothing about the driver. */
        read_pixel(SIDE / 2, 1, px);
        const GLenum err = glGetError();
        snprintf(detail, sizeof(detail), "rgb=%u,%u,%u err=0x%04x", px[0],
                 px[1], px[2], (unsigned)err);
        result("wide_lines", prog && is_tint(px), detail);
        glDeleteProgram(prog);
        drain();
    }

    /* 5. flat_unfilled: flat_unfilled_draw's vertex capture. A flat-shaded
     * unfilled polygon's colour cannot come from the rasteriser -- GL takes it
     * from the provoking vertex of the *face*, not of the edge -- so the
     * driver replays the vertex stage to find it. */
    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_read_flat},
                                  {GL_FRAGMENT_SHADER, fs_flat}};
        GLuint prog = build(s, 2);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            glUseProgram(prog);
            glPolygonMode(GL_FRONT_AND_BACK, GL_LINE);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            glPolygonMode(GL_FRONT_AND_BACK, GL_FILL);
        }
        glFinish();
        /* The bottom edge of the outline, on the same interior row the wide
         * line covers -- not the viewport's own edge, where the reference
         * draws nothing. */
        read_pixel(SIDE / 2, 1, px);
        const GLenum err = glGetError();
        snprintf(detail, sizeof(detail), "rgb=%u,%u,%u err=0x%04x", px[0],
                 px[1], px[2], (unsigned)err);
        result("flat_unfilled", prog && is_tint(px), detail);
        glDeleteProgram(prog);
        drain();
    }

    /* 6. cull_distance: capture_positions' replay. A vertex shader that writes
     * gl_CullDistance makes the driver replay the stage as compute to decide
     * which primitives survive, and that replay is a sixth dispatch of the
     * same variant. Nothing is culled here -- the distance is positive at
     * every vertex -- so a failure is the dispatch, not the cull. */
    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_read_cull},
                                  {GL_FRAGMENT_SHADER, fs_plain}};
        GLuint prog = build(s, 2);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) { glUseProgram(prog); glDrawArrays(GL_TRIANGLES, 0, 6); }
        glFinish();
        unsigned char culled[4];
        read_pixel(SIDE / 4, SIDE / 4, px);
        read_pixel(28, 25, culled);
        const GLenum err = glGetError();
        snprintf(detail, sizeof(detail),
                 "kept=%u,%u,%u culled=%u,%u,%u err=0x%04x", px[0], px[1],
                 px[2], culled[0], culled[1], culled[2], (unsigned)err);
        /* Both halves: triangle A survives with the block's tint, and
         * triangle B is gone. A draw that lost the capture fails the first;
         * a cull that did not happen fails the second. */
        result("cull_distance",
               prog && is_tint(px) && culled[1] < 64, detail);
        glDeleteProgram(prog);
        drain();
    }

    /* 7. write_through: a vertex shader that reads one block and writes
     * another, through the geometry path. The write makes the rasterising
     * module a stripped clone and the capture a different one, so the two
     * disagree about whether block 1 is writable -- and a layout built from
     * the wrong one is rejected outright. Both the picture and the tally are
     * checked: a pass needs the read to have worked *and* the write to have
     * landed, which is what separates "the dispatch ran" from "the dispatch
     * ran with the right access". */
    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_read_write},
                                  {GL_GEOMETRY_SHADER, gs_passthrough},
                                  {GL_FRAGMENT_SHADER, fs_from_gs}};
        GLuint prog = build(s, 3);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, tally);
        glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(tally_seed),
                        &tally_seed);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) { glUseProgram(prog); glDrawArrays(GL_TRIANGLES, 0, 3); }
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
        glFinish();
        read_pixel(SIDE / 4, SIDE / 4, px);
        GLuint hits = 0;
        glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(hits), &hits);
        const GLenum err = glGetError();
        snprintf(detail, sizeof(detail), "rgb=%u,%u,%u hits=%u err=0x%04x",
                 px[0], px[1], px[2], hits, (unsigned)err);
        result("write_through", prog && is_tint(px) && hits == 3, detail);
        glDeleteProgram(prog);
        drain();
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
