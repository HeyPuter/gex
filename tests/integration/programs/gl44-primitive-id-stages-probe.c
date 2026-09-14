/* gl_PrimitiveID in a fragment shader *behind* a geometry or tessellation
 * stage.
 *
 * gl44-primitive-id-probe covers the plain draw, where every vertex derives
 * the id from its own index. That derivation cannot serve here: a geometry or
 * tessellation draw rasterises through a passthrough vertex shader reading
 * records out of a storage buffer, and its gl_VertexID is a record number,
 * not a primitive number. The emulated stage knows the primitive exactly --
 * gex_wgpu_gs_builtin computes it -- so the id has to be carried out of that
 * stage in the record rather than recomputed after it.
 *
 * Until it is, the carrier is unwritten and gl_PrimitiveID reads 0. That is a
 * wrong pixel rather than a dead process, which is why it survived: nothing
 * measured it. This does.
 *
 *   geometry        a geometry shader writing gl_PrimitiveID = gl_PrimitiveIDIn.
 *                   Three triangles in three bands, so ids 0/1/2.
 *   geometry_offset the same shader writing `gl_PrimitiveIDIn * 10 + 7`. A
 *                   driver that recovered the id from the *vertex* numbering
 *                   rather than from the shader's write would pass the case
 *                   above and fail this one -- the GS's value is the one GL
 *                   specifies, and it need not resemble any primitive count.
 *   tessellation    three patches through a passthrough control/evaluation
 *                   pair. No geometry shader, so the id is whatever GL
 *                   generates for the tessellated output; the check is that
 *                   the three bands differ and ascend, not a particular
 *                   absolute value, because that is the part the spec pins
 *                   down and the part a zeroed carrier fails.
 *   tess_geometry   both stages at once, the geometry shader writing the id.
 *                   The two emulated stages chain, and the carrier has to
 *                   survive the second as well as the first.
 *
 * Geometry taken from gl44-primitive-id-probe: three wide horizontal bands,
 * each spanning well past both edges at its base. The first attempt at that
 * probe used thin triangles and sampled outside them, so the *reference*
 * failed -- see its header.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define SIDE 24

static int cases;
static int passed;

static void result(const char *name, int ok, const char *detail)
{
    ++cases;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-16s %s\n", name, detail);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 400 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
/* Plus one, so that zero means "nothing was drawn here" and can be told from
 * primitive zero -- which is exactly the value an unwritten carrier gives. */
static const char *fs_src =
    "#version 400 core\n"
    "out uvec4 colour;\n"
    "void main() { colour = uvec4(uint(gl_PrimitiveID) + 1u, 0u, 0u, 1u); }\n";
static const char *gs_same =
    "#version 400 core\n"
    "layout(triangles) in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "void main() {\n"
    "  for (int i = 0; i < 3; ++i) {\n"
    "    gl_Position = gl_in[i].gl_Position;\n"
    "    gl_PrimitiveID = gl_PrimitiveIDIn;\n"
    "    EmitVertex();\n"
    "  }\n"
    "  EndPrimitive();\n"
    "}\n";
static const char *gs_offset =
    "#version 400 core\n"
    "layout(triangles) in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "void main() {\n"
    "  for (int i = 0; i < 3; ++i) {\n"
    "    gl_Position = gl_in[i].gl_Position;\n"
    "    gl_PrimitiveID = gl_PrimitiveIDIn * 10 + 7;\n"
    "    EmitVertex();\n"
    "  }\n"
    "  EndPrimitive();\n"
    "}\n";
static const char *tcs_src =
    "#version 400 core\n"
    "layout(vertices = 3) out;\n"
    "void main() {\n"
    "  gl_out[gl_InvocationID].gl_Position =\n"
    "    gl_in[gl_InvocationID].gl_Position;\n"
    "  gl_TessLevelOuter[0] = 1.0;\n"
    "  gl_TessLevelOuter[1] = 1.0;\n"
    "  gl_TessLevelOuter[2] = 1.0;\n"
    "  gl_TessLevelInner[0] = 1.0;\n"
    "}\n";
static const char *tes_src =
    "#version 400 core\n"
    "layout(triangles, equal_spacing, ccw) in;\n"
    "void main() {\n"
    "  gl_Position = gl_TessCoord.x * gl_in[0].gl_Position +\n"
    "                gl_TessCoord.y * gl_in[1].gl_Position +\n"
    "                gl_TessCoord.z * gl_in[2].gl_Position;\n"
    "}\n";

/* Three wide triangles, one per horizontal band and none overlapping. */
static const float tri_pos[9][2] = {
    {-2.0f, -0.95f}, {2.0f, -0.95f}, {0.0f, -0.40f},
    {-2.0f, -0.28f}, {2.0f, -0.28f}, {0.0f,  0.28f},
    {-2.0f,  0.40f}, {2.0f,  0.40f}, {0.0f,  0.95f},
};

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

/* Draws the three bands and reads one pixel out of each. */
static void run(GLuint prog, int patches, GLuint got[3])
{
    const GLuint zero[4] = {0, 0, 0, 0};
    got[0] = got[1] = got[2] = 0;
    glClearBufferuiv(GL_COLOR, 0, zero);
    if (!prog)
        return;
    glUseProgram(prog);
    if (patches) {
        glPatchParameteri(GL_PATCH_VERTICES, 3);
        glDrawArrays(GL_PATCHES, 0, 9);
    } else {
        glDrawArrays(GL_TRIANGLES, 0, 9);
    }
    glFinish();
    const int y[3] = {SIDE / 8, SIDE / 2, 7 * SIDE / 8};
    for (unsigned i = 0; i < 3; ++i)
        glReadPixels(SIDE / 2, y[i], 1, 1, GL_RED_INTEGER, GL_UNSIGNED_INT,
                     &got[i]);
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
                                       EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 2; }
    static const EGLint surf_attrs[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE,
                                        EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, surf_attrs);
    eglMakeCurrent(dpy, surf, surf, ctx);
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint vao = 0, vbo = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof tri_pos, tri_pos, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    GLuint tex = 0, fbo = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R32UI, SIDE, SIDE, 0, GL_RED_INTEGER,
                 GL_UNSIGNED_INT, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, SIDE, SIDE);
    glDisable(GL_DEPTH_TEST);

    GLuint got[3];
    char detail[160];

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_src},
                                  {GL_GEOMETRY_SHADER, gs_same},
                                  {GL_FRAGMENT_SHADER, fs_src}};
        GLuint prog = build(s, 3);
        run(prog, 0, got);
        snprintf(detail, sizeof(detail), "ids=%u,%u,%u want 1,2,3 err=0x%04x",
                 got[0], got[1], got[2], (unsigned)glGetError());
        result("geometry", prog && got[0] == 1 && got[1] == 2 && got[2] == 3,
               detail);
        glDeleteProgram(prog);
    }

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_src},
                                  {GL_GEOMETRY_SHADER, gs_offset},
                                  {GL_FRAGMENT_SHADER, fs_src}};
        GLuint prog = build(s, 3);
        run(prog, 0, got);
        snprintf(detail, sizeof(detail), "ids=%u,%u,%u want 8,18,28 err=0x%04x",
                 got[0], got[1], got[2], (unsigned)glGetError());
        result("geometry_offset",
               prog && got[0] == 8 && got[1] == 18 && got[2] == 28, detail);
        glDeleteProgram(prog);
    }

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_src},
                                  {GL_TESS_CONTROL_SHADER, tcs_src},
                                  {GL_TESS_EVALUATION_SHADER, tes_src},
                                  {GL_FRAGMENT_SHADER, fs_src}};
        GLuint prog = build(s, 4);
        run(prog, 1, got);
        snprintf(detail, sizeof(detail), "ids=%u,%u,%u want distinct ascending "
                 "and non-zero err=0x%04x", got[0], got[1], got[2],
                 (unsigned)glGetError());
        /* Distinct and ascending rather than a fixed triple. Without a
         * geometry shader GL generates the id itself, and pinning the
         * absolute value would be asserting a reading of the spec rather than
         * measuring the driver. An unwritten carrier gives 1,1,1 -- every
         * band reporting primitive zero -- which this catches either way. */
        result("tessellation",
               prog && got[0] > 0 && got[0] < got[1] && got[1] < got[2],
               detail);
        glDeleteProgram(prog);
    }

    {
        const struct stage s[] = {{GL_VERTEX_SHADER, vs_src},
                                  {GL_TESS_CONTROL_SHADER, tcs_src},
                                  {GL_TESS_EVALUATION_SHADER, tes_src},
                                  {GL_GEOMETRY_SHADER, gs_same},
                                  {GL_FRAGMENT_SHADER, fs_src}};
        GLuint prog = build(s, 5);
        run(prog, 1, got);
        snprintf(detail, sizeof(detail), "ids=%u,%u,%u want distinct ascending "
                 "and non-zero err=0x%04x", got[0], got[1], got[2],
                 (unsigned)glGetError());
        result("tess_geometry",
               prog && got[0] > 0 && got[0] < got[1] && got[1] < got[2],
               detail);
        glDeleteProgram(prog);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
