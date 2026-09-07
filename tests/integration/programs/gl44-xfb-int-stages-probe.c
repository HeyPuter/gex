/* Integer transform-feedback varyings out of a geometry shader, with and
 * without tessellation ahead of it, with and without a sampler in every stage.
 *
 * KHR-GL44.texture_view.reference_counting -- the case at
 * gl4cTextureViewTests.cpp:5467 -- fails on `Program linking failed.` and
 * prints no reason: the CTS does not dump the info log there and the driver
 * names no refusal, so the only way to see it is to build the same shape and
 * ask. Its program is vertex + tessellation control + tessellation evaluation
 * + geometry + fragment, every stage sampling a texture, with four **int**
 * varyings captured by transform feedback from the geometry stage:
 *
 *     out int  gs_fs_vs_sampling_result;
 *     out int  gs_fs_tc_sampling_result;
 *     out int  gs_fs_te_sampling_result;
 *     out int  gs_fs_gs_sampling_result;
 *
 * Three programs, growing, so a failure names which ingredient did it rather
 * than only that the combination fails: integer captures alone, then the same
 * behind tessellation, then the same with a sampler read in every stage. The
 * info log is printed for whichever one breaks.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases, passed;

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-40s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2048] = {0};
               glGetShaderInfoLog(sh, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s compile failed: %s\n", what, log); }
    return ok ? sh : 0;
}

/* The four names the CTS captures, in its order. */
static const char *varyings[4] = {
    "gs_fs_vs_sampling_result", "gs_fs_tc_sampling_result",
    "gs_fs_te_sampling_result", "gs_fs_gs_sampling_result"
};

#define SAMPLER_VS(use) (use ? "uniform sampler2D tex;\n" : "")
#define FETCH(use, into) (use ? into : "")

static const char *vs_plain =
    "#version 400\n"
    "out int vs_result;\n"
    "void main() { vs_result = 1;\n"
    "              gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
static const char *vs_sampling =
    "#version 400\n"
    "uniform sampler2D tex;\n"
    "out int vs_result;\n"
    "void main() {\n"
    "    vs_result = (textureLod(tex, vec2(0.5), 0.0).r > 0.0) ? 1 : 0;\n"
    "    gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";

static const char *tcs_src =
    "#version 400\n"
    "layout(vertices = 3) out;\n"
    "SAMPLER"
    "in  int vs_result[];\n"
    "out int tc_vs_result[];\n"
    "out int tc_result[];\n"
    "void main() {\n"
    "    tc_vs_result[gl_InvocationID] = vs_result[0];\n"
    "    tc_result[gl_InvocationID] = FETCH;\n"
    "    gl_out[gl_InvocationID].gl_Position = gl_in[0].gl_Position;\n"
    "    gl_TessLevelOuter[0] = 1.0; gl_TessLevelOuter[1] = 1.0;\n"
    "    gl_TessLevelOuter[2] = 1.0; gl_TessLevelInner[0] = 1.0;\n"
    "}\n";
static const char *tes_src =
    "#version 400\n"
    "layout(triangles, equal_spacing, ccw) in;\n"
    "SAMPLER"
    "in  int tc_vs_result[];\n"
    "in  int tc_result[];\n"
    "out int te_vs_result;\n"
    "out int te_tc_result;\n"
    "out int te_result;\n"
    "void main() {\n"
    "    te_vs_result = tc_vs_result[0];\n"
    "    te_tc_result = tc_result[0];\n"
    "    te_result = FETCH;\n"
    "    gl_Position = vec4(gl_TessCoord.xy * 2.0 - 1.0, 0.0, 1.0);\n"
    "}\n";
/* After tessellation: three inputs per triangle. */
static const char *gs_after_tess =
    "#version 400\n"
    "layout(triangles)                        in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "SAMPLER"
    "in  int te_vs_result[];\n"
    "in  int te_tc_result[];\n"
    "in  int te_result[];\n"
    "flat out int gs_fs_vs_sampling_result;\n"
    "flat out int gs_fs_tc_sampling_result;\n"
    "flat out int gs_fs_te_sampling_result;\n"
    "flat out int gs_fs_gs_sampling_result;\n"
    "void main() {\n"
    "    for (int i = 0; i < 3; ++i) {\n"
    "        gs_fs_vs_sampling_result = te_vs_result[0] & te_vs_result[1]\n"
    "                                 & te_vs_result[2];\n"
    "        gs_fs_tc_sampling_result = te_tc_result[0] & te_tc_result[1]\n"
    "                                 & te_tc_result[2];\n"
    "        gs_fs_te_sampling_result = te_result[0] & te_result[1]\n"
    "                                 & te_result[2];\n"
    "        gs_fs_gs_sampling_result = FETCH;\n"
    "        gl_Position = gl_in[i].gl_Position;\n"
    "        EmitVertex();\n"
    "    }\n"
    "}\n";
/* Without tessellation: the vertex stage feeds the geometry stage directly. */
static const char *gs_direct =
    "#version 400\n"
    "layout(points)                           in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "SAMPLER"
    "in  int vs_result[];\n"
    "flat out int gs_fs_vs_sampling_result;\n"
    "flat out int gs_fs_tc_sampling_result;\n"
    "flat out int gs_fs_te_sampling_result;\n"
    "flat out int gs_fs_gs_sampling_result;\n"
    "void main() {\n"
    "    for (int i = 0; i < 3; ++i) {\n"
    "        gs_fs_vs_sampling_result = vs_result[0];\n"
    "        gs_fs_tc_sampling_result = 1;\n"
    "        gs_fs_te_sampling_result = 1;\n"
    "        gs_fs_gs_sampling_result = FETCH;\n"
    "        gl_Position = vec4(float(i) - 1.0, -1.0, 0.0, 1.0);\n"
    "        EmitVertex();\n"
    "    }\n"
    "}\n";
static const char *fs_src =
    "#version 400\n"
    "flat in int gs_fs_vs_sampling_result;\n"
    "flat in int gs_fs_tc_sampling_result;\n"
    "flat in int gs_fs_te_sampling_result;\n"
    "flat in int gs_fs_gs_sampling_result;\n"
    "out vec4 colour;\n"
    "void main() {\n"
    "    colour = vec4(float(gs_fs_vs_sampling_result\n"
    "                      & gs_fs_tc_sampling_result\n"
    "                      & gs_fs_te_sampling_result\n"
    "                      & gs_fs_gs_sampling_result), 0, 0, 1);\n"
    "}\n";

/* Substitute SAMPLER and FETCH, which is how the CTS builds these too. */
static char scratch[4][8192];
static const char *expand(unsigned slot, const char *src, int sampling) {
    char *w = scratch[slot];
    const char *p = src;
    *w = '\0';
    while (*p) {
        if (strncmp(p, "SAMPLER", 7) == 0) {
            const char *decl = sampling ? "uniform sampler2D tex;\n" : "";
            strcpy(w, decl); w += strlen(decl); p += 7;
        } else if (strncmp(p, "FETCH", 5) == 0) {
            const char *fetch = sampling
                ? "((textureLod(tex, vec2(0.5), 0.0).r > 0.0) ? 1 : 0)" : "1";
            strcpy(w, fetch); w += strlen(fetch); p += 5;
        } else {
            *w++ = *p++;
        }
    }
    *w = '\0';
    return scratch[slot];
}

static int link_program(const char *label, int tessellated, int sampling) {
    GLuint program = glCreateProgram();
    GLuint vs = compile(GL_VERTEX_SHADER,
                        sampling ? vs_sampling : vs_plain, "vs");
    if (!vs) return -1;
    glAttachShader(program, vs);
    if (tessellated) {
        GLuint tcs = compile(GL_TESS_CONTROL_SHADER,
                             expand(0, tcs_src, sampling), "tcs");
        GLuint tes = compile(GL_TESS_EVALUATION_SHADER,
                             expand(1, tes_src, sampling), "tes");
        if (!tcs || !tes) return -1;
        glAttachShader(program, tcs); glAttachShader(program, tes);
    }
    GLuint gs = compile(GL_GEOMETRY_SHADER,
                        expand(2, tessellated ? gs_after_tess : gs_direct,
                               sampling), "gs");
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!gs || !fs) return -1;
    glAttachShader(program, gs); glAttachShader(program, fs);

    glTransformFeedbackVaryings(program, 4, varyings, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[4096] = {0};
        glGetProgramInfoLog(program, sizeof log, NULL, log);
        fprintf(stderr, "INFO %s link log: %s\n", label, log);
    }
    while (glGetError() != GL_NO_ERROR) { }
    return linked ? 1 : 0;
}


/* The CTS's sampler declaration is a single sampler whose *type* changes per
   iteration -- sampler1D, sampler1DArray, sampler2D, sampler2DArray,
   sampler2DMS and the multisample array -- so the link failure may be about
   which sampler type an emulated stage can hold rather than about the
   captures at all. These two are the interesting pair: WGSL has
   texture_multisampled_2d and no texture_multisampled_2d_array, and WebGPU
   requires a multisampled texture to have exactly one array layer. */
static const char *gs_sampler_kind =
    "#version 400\n"
    "layout(points)                           in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "uniform SAMPLERTYPE tex;\n"
    "in  int vs_result[];\n"
    "flat out int gs_fs_vs_sampling_result;\n"
    "flat out int gs_fs_tc_sampling_result;\n"
    "flat out int gs_fs_te_sampling_result;\n"
    "flat out int gs_fs_gs_sampling_result;\n"
    "void main() {\n"
    "    for (int i = 0; i < 3; ++i) {\n"
    "        gs_fs_vs_sampling_result = vs_result[0];\n"
    "        gs_fs_tc_sampling_result = 1;\n"
    "        gs_fs_te_sampling_result = 1;\n"
    "        gs_fs_gs_sampling_result = (FETCHMS.r > 0.0) ? 1 : 0;\n"
    "        gl_Position = vec4(float(i) - 1.0, -1.0, 0.0, 1.0);\n"
    "        EmitVertex();\n"
    "    }\n"
    "}\n";

static char kind_scratch[8192];
static int link_sampler_kind(const char *label, const char *type,
                             const char *fetch) {
    char *w = kind_scratch; const char *p = gs_sampler_kind;
    while (*p) {
        if (strncmp(p, "SAMPLERTYPE", 11) == 0) {
            strcpy(w, type); w += strlen(type); p += 11;
        } else if (strncmp(p, "FETCHMS", 7) == 0) {
            strcpy(w, fetch); w += strlen(fetch); p += 7;
        } else { *w++ = *p++; }
    }
    *w = '\0';
    GLuint program = glCreateProgram();
    GLuint vs = compile(GL_VERTEX_SHADER, vs_plain, "vs");
    GLuint gs = compile(GL_GEOMETRY_SHADER, kind_scratch, label);
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !gs || !fs) return -1;
    glAttachShader(program, vs); glAttachShader(program, gs);
    glAttachShader(program, fs);
    glTransformFeedbackVaryings(program, 4, varyings, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[4096] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO %s link log: %s\n", label, log); }
    while (glGetError() != GL_NO_ERROR) { }
    return linked ? 1 : 0;
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    eq("four_int_captures_from_geometry", link_program(
           "geometry only", 0, 0), 1);
    eq("four_int_captures_after_tessellation", link_program(
           "after tessellation", 1, 0), 1);
    eq("and_a_sampler_in_every_stage", link_program(
           "sampling in every stage", 1, 1), 1);

    /* Which sampler type an emulated geometry stage can hold. */
    eq("geometry_holds_sampler2DMS", link_sampler_kind(
           "sampler2DMS", "sampler2DMS",
           "texelFetch(tex, ivec2(0), 0)"), 1);
    /* sampler2DMSArray is *reported*, not asserted. WebGPU requires a
       multisampled texture to have exactly one array layer and WGSL has no
       texture_multisampled_2d_array, so this cannot link here and the
       reference links it fine -- an assertion would be a permanent
       disagreement in tests/gates/probe-suite.sh, where the value of the
       expected-difference list is that an unexpected entry means a real
       regression. Recorded so that a future platform gaining the type is
       still visible in the log.

       This is the whole of what KHR-GL44.texture_view has left: two cases
       fail allocating a GL_TEXTURE_2D_MULTISAMPLE_ARRAY, and the two that
       fail on `Program linking failed.` are this shader, because the CTS
       varies its one sampler's *type* per iteration and reaches
       sampler2DMSArray. Four failures, one wall. */
    {
        const int linked = link_sampler_kind(
            "sampler2DMSArray", "sampler2DMSArray",
            "texelFetch(tex, ivec3(0), 0)");
        fprintf(stderr, "INFO %-40s linked=%d (a platform ceiling here, "
                "not a defect)\n", "geometry_holds_sampler2DMSArray", linked);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
