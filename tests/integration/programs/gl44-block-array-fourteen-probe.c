/* Fourteen uniform blocks in one array, at bindings 2..15, read and verified.
 *
 * This is the shape KHR-GL44.shading_language_420pack.binding_uniform_block_array
 * actually uses: `uniform GOKU { vec4 gohan; vec4 goten; } goku[14];` at
 * `binding = 2`, so the highest binding is 15 and the stage needs sixteen
 * uniform buffer bindings. Every earlier block-array probe here used three,
 * which is why all four of them passed on both paths while the CTS case kept
 * failing in the browser: three bindings clear any limit, sixteen do not.
 * WebGPU guarantees only 12 uniform buffers per shader stage, and the page's
 * request for 16 is clamped by Math.min(available, wanted), so it can quietly
 * settle for less.
 *
 * The limits are reported as cases of their own, because whether the driver
 * *advertises* sixteen bindings and whether a program can *use* them are
 * separate claims and only the second one is what the CTS checks.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases, passed;

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-40s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static void at_least(const char *name, long long got, long long floor) {
    ++cases;
    const int ok = got >= floor;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-40s got=%lld want>=%lld\n", name, got, floor);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* The CTS's own data: element i holds gohan = the low three bits of i as
 * floats with w = 0, and goten = the same with w = 1. */
static void element_data(unsigned i, float out[8]) {
    const float b2 = (float)((i >> 2) & 1), b1 = (float)((i >> 1) & 1),
                b0 = (float)(i & 1);
    out[0] = b2; out[1] = b1; out[2] = b0; out[3] = 0.0f;
    out[4] = b2; out[5] = b1; out[6] = b0; out[7] = 1.0f;
}

/* The CTS's own form: twenty-eight constant-index reads, no loop. A dynamic
   index is a different question and is asked separately below, because it
   fails the link and would otherwise take this program down with it. */
static char vs_src_buffer[8192];
static const char *build_constant_index_vs(void) {
    char *w = vs_src_buffer;
    w += sprintf(w,
        "#version 420 core\n"
        "layout(std140, binding = 2) uniform GOKU {\n"
        "    vec4 gohan;\n"
        "    vec4 goten;\n"
        "} goku[14];\n"
        "out float verdict;\n"
        "void main() {\n"
        "    int good = 0;\n");
    for (unsigned i = 0; i < 14; ++i) {
        const int b2 = (i >> 2) & 1, b1 = (i >> 1) & 1, b0 = i & 1;
        w += sprintf(w,
            "    if (goku[%u].gohan == vec4(%d.0, %d.0, %d.0, 0.0)) ++good;\n"
            "    if (goku[%u].goten == vec4(%d.0, %d.0, %d.0, 1.0)) ++good;\n",
            i, b2, b1, b0, i, b2, b1, b0);
    }
    w += sprintf(w,
        "    verdict = float(good);\n"
        "    gl_Position = vec4((gl_VertexID == 1) ? 3.0 : -1.0,\n"
        "                       (gl_VertexID == 2) ? 3.0 : -1.0, 0.0, 1.0);\n"
        "}\n");
    return vs_src_buffer;
}

/* The same array read through a loop counter. GLSL 4.20 allows a dynamically
   uniform index into a uniform block array, so this must link. */
static const char *vs_dynamic_src =
    "#version 420 core\n"
    "layout(std140, binding = 2) uniform GOKU {\n"
    "    vec4 gohan;\n"
    "    vec4 goten;\n"
    "} goku[14];\n"
    "out float verdict;\n"
    "void main() {\n"
    "    int good = 0;\n"
    "    for (int i = 0; i < 14; ++i) {\n"
    "        vec3 want = vec3(float((i >> 2) & 1), float((i >> 1) & 1),\n"
    "                         float(i & 1));\n"
    "        if (goku[i].gohan == vec4(want, 0.0)) ++good;\n"
    "        if (goku[i].goten == vec4(want, 1.0)) ++good;\n"
    "    }\n"
    "    verdict = float(good);\n"
    "    gl_Position = vec4((gl_VertexID == 1) ? 3.0 : -1.0,\n"
    "                       (gl_VertexID == 2) ? 3.0 : -1.0, 0.0, 1.0);\n"
    "}\n";

static const char *fs_src =
    "#version 420 core\n"
    "in float verdict;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(verdict / 255.0, 0.0, 0.0, 1.0); }\n";

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2048] = {0};
               glGetShaderInfoLog(sh, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s compile failed: %s\n", what, log); }
    return ok ? sh : 0;
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

    GLint bindings = 0, vs_blocks = 0, fs_blocks = 0;
    glGetIntegerv(GL_MAX_UNIFORM_BUFFER_BINDINGS, &bindings);
    glGetIntegerv(GL_MAX_VERTEX_UNIFORM_BLOCKS, &vs_blocks);
    glGetIntegerv(GL_MAX_FRAGMENT_UNIFORM_BLOCKS, &fs_blocks);
    /* The CTS needs bindings 2..15 to exist and one stage to hold 14 blocks. */
    at_least("max_uniform_buffer_bindings_is_16", bindings, 16);
    at_least("max_vertex_uniform_blocks_is_14", vs_blocks, 14);
    fprintf(stderr, "INFO fragment_uniform_blocks=%d\n", fs_blocks);

    GLuint vs = compile(GL_VERTEX_SHADER, build_constant_index_vs(), "vs");
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !fs) { printf("SUMMARY cases=%d passed=%d failed=%d\n",
                             cases + 1, passed, cases + 1 - passed);
                      printf("CASE fourteen_blocks_compile FAIL\n"); return 1; }
    ++cases; ++passed; printf("CASE fourteen_blocks_compile PASS\n");

    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[2048] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log); }
    eq("fourteen_blocks_link", linked, 1);
    if (!linked) { printf("SUMMARY cases=%d passed=%d failed=%d\n", cases,
                          passed, cases - passed); return 1; }

    /* Every element's binding must be its index plus two, as the CTS checks. */
    int wrong_binding = 0;
    for (unsigned i = 0; i < 14; ++i) {
        char name[32]; snprintf(name, sizeof name, "GOKU[%u]", i);
        const GLuint index = glGetUniformBlockIndex(program, name);
        GLint binding = -1;
        if (index != GL_INVALID_INDEX)
            glGetActiveUniformBlockiv(program, index, GL_UNIFORM_BLOCK_BINDING,
                                      &binding);
        if (binding != (GLint)(i + 2)) {
            ++wrong_binding;
            fprintf(stderr, "INFO %s index=%u binding=%d want=%u\n", name,
                    index, binding, i + 2);
        }
    }
    eq("every_element_binding_is_index_plus_2", wrong_binding, 0);

    GLuint ubo[14];
    glGenBuffers(14, ubo);
    for (unsigned i = 0; i < 14; ++i) {
        float block[8]; element_data(i, block);
        glBindBuffer(GL_UNIFORM_BUFFER, ubo[i]);
        glBufferData(GL_UNIFORM_BUFFER, sizeof block, block, GL_STATIC_DRAW);
        /* bindRange, as the CTS does, not bindBufferBase. */
        glBindBufferRange(GL_UNIFORM_BUFFER, i + 2, ubo[i], 0, sizeof block);
    }
    eq("binding_fourteen_buffers_is_clean", drain(), GL_NO_ERROR);

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glUseProgram(program);
    glViewport(0, 0, 16, 16);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    eq("fourteen_block_draw_is_clean", drain(), GL_NO_ERROR);

    unsigned char pixel[4] = {0};
    glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    /* 28 of 28 vec4s correct. A smaller number says how many arrived. */
    eq("all_28_vec4s_arrived", pixel[0], 28);

    /* Now the same array through a loop counter, as its own program so its
       link failure cannot affect the constant-index result above. */
    {
        GLuint dvs = compile(GL_VERTEX_SHADER, vs_dynamic_src, "dynamic vs");
        GLint dlinked = 0;
        if (dvs) {
            GLuint dprog = glCreateProgram();
            glAttachShader(dprog, dvs); glAttachShader(dprog, fs);
            glLinkProgram(dprog);
            glGetProgramiv(dprog, GL_LINK_STATUS, &dlinked);
            if (!dlinked) { char log[2048] = {0};
                            glGetProgramInfoLog(dprog, sizeof log, NULL, log);
                            fprintf(stderr, "INFO dynamic link failed: %s\n",
                                    log); }
        }
        eq("dynamic_index_block_array_links", dlinked, 1);
        if (dlinked) {
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            unsigned char dp[4] = {0};
            glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, dp);
            eq("dynamic_index_reads_all_28", dp[0], 28);
        } else {
            eq("dynamic_index_reads_all_28", -1, 28);
        }
        drain();
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
