/* ARB_shader_draw_parameters in a vertex shader that is replayed as compute.
 *
 * gl_BaseVertex, gl_BaseInstance and gl_DrawID reach the rasterizing copy of a
 * vertex shader through a block of their own, declared *after* the capture and
 * varying clones are taken -- deliberately, because a compute variant binds
 * its own groups and a storage block nothing fills would invalidate its
 * pipeline. The consequence was that the clones kept the raw intrinsics and
 * nothing lowered them, so every one of the three read **zero** in a captured
 * vertex shader.
 *
 * That is the quiet half of the failure mode the capture paths keep producing.
 * Its loud half -- an unbound storage block -- kills the command buffer and
 * says so; this one draws, and is simply wrong. A draw through a geometry or
 * tessellation stage put its geometry in the wrong place while the identical
 * shader drawn plainly was right, and nothing measured it.
 *
 *   plain           the ordinary draw, as a control. gl_BaseVertex is read
 *                   from the rasterizing copy's own block here.
 *   geometry        through a geometry shader   -- gs_draw's vertex capture
 *   tessellation    through a tessellation pair -- tess_draw's vertex capture
 *   wide_lines      glLineWidth > 1             -- line_draw's vertex capture
 *   flat_unfilled   flat shading + GL_LINE      -- flat_unfilled_draw's
 *   base_instance   gl_BaseInstance through the geometry path
 *   draw_id         gl_DrawID through the geometry path, with two commands in
 *                   one glMultiDrawArrays so the two ids differ
 *
 * Each case reads the parameter and turns it into a colour, so a zero is a
 * black pixel and a right answer is the tint -- "the draw happened" and "the
 * draw read the right value" are separate answers, which is what
 * zero-means-nobody-wrote is about.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

#define SIDE 32

static int cases;
static int passed;

static void result(const char *name, int ok, const char *detail)
{
    ++cases;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-16s %s\n", name, detail);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* Green only when the parameter is the expected value, so the pixel says
 * which of "nothing drew" and "the value was wrong" happened -- both give
 * black, but the err= and the other cases separate them. */
static const char *vs_base_vertex =
    "#version 460 core\n"
    "#extension GL_ARB_shader_draw_parameters : require\n"
    "layout(location = 0) in vec2 p;\n"
    "out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "  tint = vec3(0.0, gl_BaseVertexARB == 3 ? 1.0 : 0.0, 0.5);\n"
    "}\n";
static const char *vs_base_vertex_flat =
    "#version 460 core\n"
    "#extension GL_ARB_shader_draw_parameters : require\n"
    "layout(location = 0) in vec2 p;\n"
    "flat out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "  tint = vec3(0.0, gl_BaseVertexARB == 3 ? 1.0 : 0.0, 0.5);\n"
    "}\n";
static const char *vs_base_instance =
    "#version 460 core\n"
    "#extension GL_ARB_shader_draw_parameters : require\n"
    "layout(location = 0) in vec2 p;\n"
    "out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "  tint = vec3(0.0, gl_BaseInstanceARB == 5 ? 1.0 : 0.0, 0.5);\n"
    "}\n";
/* Two commands, ids 0 and 1, drawing a small triangle that command 1 shifts
 * a whole unit to the right.
 *
 * The shift is what makes the case discriminate, and the first version did not
 * have it in a useful direction: it moved command 1 off the bottom, so an id
 * stuck at 0 drew command 1 exactly on top of command 0 and the sampled pixel
 * was the same either way. The case passed against a driver that hardcoded
 * gl_DrawID to zero. Now the two commands land in different halves, and both
 * halves are read: a stuck id leaves the right half black. */
static const char *vs_draw_id =
    "#version 460 core\n"
    "#extension GL_ARB_shader_draw_parameters : require\n"
    "layout(location = 0) in vec2 p;\n"
    "out vec3 tint;\n"
    "void main() {\n"
    "  gl_Position = vec4(p.x + float(gl_DrawIDARB), p.y, 0.0, 1.0);\n"
    "  tint = vec3(0.0, 1.0, float(gl_DrawIDARB) * 0.5 + 0.25);\n"
    "}\n";
static const char *fs_plain =
    "#version 460 core\n"
    "in vec3 tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(tint, 1.0); }\n";
static const char *fs_flat =
    "#version 460 core\n"
    "flat in vec3 tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(tint, 1.0); }\n";
static const char *gs_passthrough =
    "#version 460 core\n"
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
    "#version 460 core\n"
    "in vec3 gtint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(gtint, 1.0); }\n";
static const char *tcs_passthrough =
    "#version 460 core\n"
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
    "#version 460 core\n"
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
static const char *fs_from_tes =
    "#version 460 core\n"
    "in vec3 etint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(etint, 1.0); }\n";

struct stage { GLenum type; const char *src; };

static GLuint build(const struct stage *stages, unsigned count)
{
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
            fprintf(stderr, "INFO compile (0x%x): %s\n",
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

static int is_tint(const unsigned char *p)
{
    return p[0] < 32 && p[1] > 200 && p[2] > 96 && p[2] < 160;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(dpy, NULL, NULL)) { printf("FATAL eglInitialize\n"); return 2; }
    static const EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                       EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                                       EGL_NONE};
    EGLConfig cfg; EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    static const EGLint ctx_attrs[] = {EGL_CONTEXT_MAJOR_VERSION, 4,
                                       EGL_CONTEXT_MINOR_VERSION, 6, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 2; }
    static const EGLint surf_attrs[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE,
                                        EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, surf_attrs);
    eglMakeCurrent(dpy, surf, surf, ctx);
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint vao = 0, vbo = 0, ibo = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    /* Three padding vertices, then the triangle. basevertex 3 is what selects
     * it, so nothing is in the right place unless gl_BaseVertex arrived. */
    static const float pos[] = {
        9.0f, 9.0f,  9.0f, 9.0f,  9.0f, 9.0f,
        -0.9f, -0.9f,  0.9f, -0.9f,  -0.9f, 0.9f,
        /* A small triangle in the bottom-left quadrant, for the draw_id case
         * alone: it has to fit in one half so that the shifted copy of it
         * lands in the other. */
        -0.9f, -0.9f,  -0.1f, -0.9f,  -0.9f, -0.1f,
    };
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof pos, pos, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    static const unsigned short idx[3] = {0, 1, 2};
    glGenBuffers(1, &ibo);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo);
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof idx, idx, GL_STATIC_DRAW);

    GLuint tex = 0, fbo = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, SIDE, SIDE);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    drain();

    unsigned char px[4];
    char detail[160];

#define CASE_BEGIN() glClear(GL_COLOR_BUFFER_BIT)
#define CASE_END(name, cond)                                                  \
    do {                                                                      \
        const GLenum err = glGetError();                                      \
        snprintf(detail, sizeof(detail), "rgb=%u,%u,%u err=0x%04x", px[0],    \
                 px[1], px[2], (unsigned)err);                                \
        result(name, (cond), detail);                                         \
        drain();                                                              \
    } while (0)

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_base_vertex},
                                  {GL_FRAGMENT_SHADER, fs_plain}};
        GLuint prog = build(s, 2);
        CASE_BEGIN();
        if (prog) {
            glUseProgram(prog);
            glDrawElementsBaseVertex(GL_TRIANGLES, 3, GL_UNSIGNED_SHORT, NULL, 3);
        }
        glFinish();
        glReadPixels(SIDE / 4, SIDE / 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        CASE_END("plain", prog && is_tint(px));
        glDeleteProgram(prog);
    }

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_base_vertex},
                                  {GL_GEOMETRY_SHADER, gs_passthrough},
                                  {GL_FRAGMENT_SHADER, fs_from_gs}};
        GLuint prog = build(s, 3);
        CASE_BEGIN();
        if (prog) {
            glUseProgram(prog);
            glDrawElementsBaseVertex(GL_TRIANGLES, 3, GL_UNSIGNED_SHORT, NULL, 3);
        }
        glFinish();
        glReadPixels(SIDE / 4, SIDE / 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        CASE_END("geometry", prog && is_tint(px));
        glDeleteProgram(prog);
    }

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_base_vertex},
                                  {GL_TESS_CONTROL_SHADER, tcs_passthrough},
                                  {GL_TESS_EVALUATION_SHADER, tes_passthrough},
                                  {GL_FRAGMENT_SHADER, fs_from_tes}};
        GLuint prog = build(s, 4);
        CASE_BEGIN();
        if (prog) {
            glUseProgram(prog);
            glPatchParameteri(GL_PATCH_VERTICES, 3);
            glDrawElementsBaseVertex(GL_PATCHES, 3, GL_UNSIGNED_SHORT, NULL, 3);
        }
        glFinish();
        glReadPixels(SIDE / 4, SIDE / 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        CASE_END("tessellation", prog && is_tint(px));
        glDeleteProgram(prog);
    }

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_base_vertex},
                                  {GL_FRAGMENT_SHADER, fs_plain}};
        GLuint prog = build(s, 2);
        CASE_BEGIN();
        if (prog) {
            glUseProgram(prog);
            glLineWidth(5.0f);
            glDrawElementsBaseVertex(GL_LINES, 2, GL_UNSIGNED_SHORT, NULL, 3);
            glLineWidth(1.0f);
        }
        glFinish();
        /* The line runs along y = -0.9, which the reference rasterises into
         * row 1; five pixels wide, so row 1 is well inside it. */
        glReadPixels(SIDE / 2, 1, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        CASE_END("wide_lines", prog && is_tint(px));
        glDeleteProgram(prog);
    }

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_base_vertex_flat},
                                  {GL_FRAGMENT_SHADER, fs_flat}};
        GLuint prog = build(s, 2);
        CASE_BEGIN();
        if (prog) {
            glUseProgram(prog);
            glPolygonMode(GL_FRONT_AND_BACK, GL_LINE);
            glDrawElementsBaseVertex(GL_TRIANGLES, 3, GL_UNSIGNED_SHORT, NULL, 3);
            glPolygonMode(GL_FRONT_AND_BACK, GL_FILL);
        }
        glFinish();
        glReadPixels(SIDE / 2, 1, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        CASE_END("flat_unfilled", prog && is_tint(px));
        glDeleteProgram(prog);
    }

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_base_instance},
                                  {GL_GEOMETRY_SHADER, gs_passthrough},
                                  {GL_FRAGMENT_SHADER, fs_from_gs}};
        GLuint prog = build(s, 3);
        CASE_BEGIN();
        if (prog) {
            glUseProgram(prog);
            /* baseinstance 5, and one instance, so the only thing the draw
             * depends on is gl_BaseInstance itself. The vertices come from
             * offset 3 in the array, which glDrawArrays' `first` gives. */
            glDrawArraysInstancedBaseInstance(GL_TRIANGLES, 3, 3, 1, 5);
        }
        glFinish();
        glReadPixels(SIDE / 4, SIDE / 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        CASE_END("base_instance", prog && is_tint(px));
        glDeleteProgram(prog);
    }

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_draw_id},
                                  {GL_GEOMETRY_SHADER, gs_passthrough},
                                  {GL_FRAGMENT_SHADER, fs_from_gs}};
        GLuint prog = build(s, 3);
        unsigned char second[4] = {0, 0, 0, 0};
        CASE_BEGIN();
        if (prog) {
            glUseProgram(prog);
            static const GLint first[2] = {6, 6};
            static const GLsizei count[2] = {3, 3};
            glMultiDrawArrays(GL_TRIANGLES, first, count, 2);
        }
        glFinish();
        /* Command 0 in the bottom-left quadrant, command 1 shifted a whole
         * clip unit right into the bottom-right. Both are read, and the blue
         * channel carries the id: 0.25 for command 0 and 0.75 for command 1.
         * An id stuck at zero puts both commands in the left half, so the
         * right sample is black and the case fails. */
        glReadPixels(6, 6, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        glReadPixels(22, 6, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, second);
        snprintf(detail, sizeof(detail),
                 "cmd0=%u,%u,%u cmd1=%u,%u,%u err=0x%04x", px[0], px[1], px[2],
                 second[0], second[1], second[2], (unsigned)glGetError());
        result("draw_id",
               prog && px[1] > 200 && px[2] > 32 && px[2] < 96 &&
               second[1] > 200 && second[2] > 160, detail);
        drain();
        glDeleteProgram(prog);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
