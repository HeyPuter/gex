/* An integer sampler in each stage that this driver runs as compute.
 *
 * WGSL cannot sample an integer texture, so the driver replaces the sample
 * with a fetch that applies the wrap, filter and swizzle itself -- and that
 * needs the *bound* sampler state, so it is a per-draw module variant rather
 * than something the base module can carry. The graphics stages and the
 * geometry stage have that machinery; whether the tessellation stages do is
 * what this asks.
 *
 * When they do not, the shader is rejected outright at link time --
 *
 *   tessellation control shader rejected: Tint rejected SPIR-V: error:
 *   sampled image must have float component type
 *
 * -- which takes the whole program, not just the sample. So each case is
 * checked for a live program *and* for the right value: a driver that fails
 * to link and one that links and reads zero look the same from the pixel
 * alone, and only one of them is this bug.
 *
 * The geometry case is the control: that stage's variant machinery exists, so
 * a failure there means something else is wrong.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define SIDE 16
#define TEXEL 77u

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLuint compile(GLenum kind, const char *src, const char *what)
{
    GLuint sh = glCreateShader(kind);
    glShaderSource(sh, 1, &src, NULL);
    glCompileShader(sh);
    GLint ok = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(sh, sizeof log, NULL, log);
        fprintf(stderr, "INFO %s compile failed: %s\n", what, log);
        glDeleteShader(sh);
        return 0;
    }
    return sh;
}

/* Returns 0 when anything failed to compile or link, which is the answer that
 * separates a rejected shader from a wrong value. */
static GLuint link_program(const char *vs, const char *tcs, const char *tes,
                           const char *gs, const char *fs, const char *what)
{
    GLuint prog = glCreateProgram();
    struct { GLenum kind; const char *src; const char *name; } parts[] = {
        {GL_VERTEX_SHADER, vs, "vs"},
        {GL_TESS_CONTROL_SHADER, tcs, "tcs"},
        {GL_TESS_EVALUATION_SHADER, tes, "tes"},
        {GL_GEOMETRY_SHADER, gs, "gs"},
        {GL_FRAGMENT_SHADER, fs, "fs"},
    };
    for (unsigned i = 0; i < 5; ++i) {
        if (!parts[i].src) continue;
        char label[64];
        snprintf(label, sizeof label, "%s %s", what, parts[i].name);
        GLuint sh = compile(parts[i].kind, parts[i].src, label);
        if (!sh) { glDeleteProgram(prog); return 0; }
        glAttachShader(prog, sh);
        glDeleteShader(sh);
    }
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetProgramInfoLog(prog, sizeof log, NULL, log);
        fprintf(stderr, "INFO %s link failed: %s\n", what, log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

/* A triangle big enough to cover the whole target from three vertices, so no
 * vertex buffer is needed and the centre pixel is always inside it. */
static const char *vs_plain =
    "#version 400 core\n"
    "const vec2 tri[3] = vec2[3](vec2(-1,-1), vec2(3,-1), vec2(-1,3));\n"
    "void main() { gl_Position = vec4(tri[gl_VertexID], 0.0, 1.0); }\n";
static const char *fs_flat =
    "#version 400 core\n"
    "flat in uint value;\n"
    "out uvec4 colour;\n"
    "void main() { colour = uvec4(value, 0u, 0u, 1u); }\n";

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

    /* The integer texture every case samples. */
    GLuint tex = 0;
    const GLuint texel = TEXEL;
    glGenTextures(1, &tex);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R32UI, 1, 1, 0, GL_RED_INTEGER,
                 GL_UNSIGNED_INT, &texel);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    GLuint out_tex = 0, fbo = 0;
    glGenTextures(1, &out_tex);
    glBindTexture(GL_TEXTURE_2D, out_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R32UI, SIDE, SIDE, 0, GL_RED_INTEGER,
                 GL_UNSIGNED_INT, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           out_tex, 0);
    glBindTexture(GL_TEXTURE_2D, tex);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n");
        return 1;
    }
    glViewport(0, 0, SIDE, SIDE);
    glDisable(GL_DEPTH_TEST);

    /* -- geometry, the control -- */
    GLuint gs_prog = link_program(
        vs_plain, NULL, NULL,
        "#version 400 core\n"
        "layout(triangles) in;\n"
        "layout(triangle_strip, max_vertices = 3) out;\n"
        "uniform usampler2D tex;\n"
        "flat out uint value;\n"
        "void main() {\n"
        "  for (int i = 0; i < 3; ++i) {\n"
        "    gl_Position = gl_in[i].gl_Position;\n"
        "    value = texture(tex, vec2(0.5)).x;\n"
        "    EmitVertex();\n"
        "  }\n"
        "  EndPrimitive();\n"
        "}\n",
        fs_flat, "gs");

    /* -- tessellation control -- */
    GLuint tcs_prog = link_program(
        vs_plain,
        "#version 400 core\n"
        "layout(vertices = 3) out;\n"
        "uniform usampler2D tex;\n"
        "patch out uint sampled;\n"
        "void main() {\n"
        "  gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;\n"
        "  if (gl_InvocationID == 0) {\n"
        "    sampled = texture(tex, vec2(0.5)).x;\n"
        "    gl_TessLevelInner[0] = 1.0;\n"
        "    gl_TessLevelOuter[0] = 1.0;\n"
        "    gl_TessLevelOuter[1] = 1.0;\n"
        "    gl_TessLevelOuter[2] = 1.0;\n"
        "  }\n"
        "}\n",
        "#version 400 core\n"
        "layout(triangles, equal_spacing, ccw) in;\n"
        "patch in uint sampled;\n"
        "flat out uint value;\n"
        "void main() {\n"
        "  gl_Position = gl_TessCoord.x * gl_in[0].gl_Position\n"
        "              + gl_TessCoord.y * gl_in[1].gl_Position\n"
        "              + gl_TessCoord.z * gl_in[2].gl_Position;\n"
        "  value = sampled;\n"
        "}\n",
        NULL, fs_flat, "tcs");

    /* -- tessellation evaluation -- */
    GLuint tes_prog = link_program(
        vs_plain,
        "#version 400 core\n"
        "layout(vertices = 3) out;\n"
        "void main() {\n"
        "  gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;\n"
        "  gl_TessLevelInner[0] = 1.0;\n"
        "  gl_TessLevelOuter[0] = 1.0;\n"
        "  gl_TessLevelOuter[1] = 1.0;\n"
        "  gl_TessLevelOuter[2] = 1.0;\n"
        "}\n",
        "#version 400 core\n"
        "layout(triangles, equal_spacing, ccw) in;\n"
        "uniform usampler2D tex;\n"
        "flat out uint value;\n"
        "void main() {\n"
        "  gl_Position = gl_TessCoord.x * gl_in[0].gl_Position\n"
        "              + gl_TessCoord.y * gl_in[1].gl_Position\n"
        "              + gl_TessCoord.z * gl_in[2].gl_Position;\n"
        "  value = texture(tex, vec2(0.5)).x;\n"
        "}\n",
        NULL, fs_flat, "tes");

    struct { const char *name; GLuint prog; int patches; } runs[] = {
        {"geometry_usampler", gs_prog, 0},
        {"tcs_usampler", tcs_prog, 1},
        {"tes_usampler", tes_prog, 1},
    };
    glPatchParameteri(GL_PATCH_VERTICES, 3);
    for (unsigned i = 0; i < 3; ++i) {
        if (!runs[i].prog) {
            fprintf(stderr, "INFO %s no program\n", runs[i].name);
            result(runs[i].name, 0);
            continue;
        }
        const GLuint zero[4] = {0, 0, 0, 0};
        glClearBufferuiv(GL_COLOR, 0, zero);
        glUseProgram(runs[i].prog);
        glUniform1i(glGetUniformLocation(runs[i].prog, "tex"), 0);
        glDrawArrays(runs[i].patches ? GL_PATCHES : GL_TRIANGLES, 0, 3);
        GLuint got = 0xffffffffu;
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RED_INTEGER,
                     GL_UNSIGNED_INT, &got);
        fprintf(stderr, "INFO %s got=%u want=%u err=0x%04x\n", runs[i].name,
                got, TEXEL, (unsigned)glGetError());
        result(runs[i].name, got == TEXEL);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
