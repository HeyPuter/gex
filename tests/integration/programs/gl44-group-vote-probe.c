/* ARB_shader_group_vote: asking a question of the invocations running together.
 *
 * `anyInvocationARB`, `allInvocationsARB` and `allInvocationsEqualARB` are
 * ballots across the group an invocation happens to be executing with -- a
 * subgroup, in every other API's language. Nothing but a subgroup operation
 * can answer them, and this driver reaches one through a chain that already
 * existed: Mesa's GLSL gives the three as NIR vote intrinsics, zink's emitter
 * has SpvOpGroupNonUniformAny/All/AllEqual, Tint takes those as WGSL's
 * subgroupAny and subgroupAll, and the device grants Subgroups.
 *
 *   `any_true`     anyInvocationARB of something exactly one invocation sees
 *                  is true
 *   `all_uniform`  allInvocationsARB of something every invocation sees is
 *                  true
 *   `all_mixed`    allInvocationsARB and allInvocationsEqualARB of something
 *                  only one invocation sees are both **false** -- the case
 *                  that distinguishes a real ballot from one taken over a
 *                  group of one, which would report true and pass everything
 *                  else
 *
 * The group size is implementation-defined and a size of one is legal, so
 * `all_mixed` is a statement about this implementation rather than about GL.
 * The reference is asked the same question and the two are compared, which is
 * the honest way round: if llvmpipe votes in groups of one it says so.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int have = have_extension("GL_ARB_shader_group_vote");
    fprintf(stderr, "INFO version=%s group_vote=%d\n", glGetString(GL_VERSION),
            have);
    if (!have) {
        fprintf(stderr, "INFO GL_ARB_shader_group_vote absent\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    static const char *cs_src =
        "#version 430\n"
        "#extension GL_ARB_shader_group_vote : require\n"
        "layout(local_size_x = 64) in;\n"
        "layout(std430, binding = 0) buffer Out { uint answer[]; };\n"
        "void main() {\n"
        "  bool mine = gl_LocalInvocationID.x == 0u;\n"
        "  bool any_mine = anyInvocationARB(mine);\n"
        "  bool all_mine = allInvocationsARB(mine);\n"
        "  bool all_true = allInvocationsARB(true);\n"
        "  bool all_same = allInvocationsEqualARB(mine);\n"
        /* One invocation reports, so the four slots are not raced for. */
        "  if (gl_LocalInvocationID.x == 0u) {\n"
        "    answer[0] = any_mine ? 1u : 0u;\n"
        "    answer[1] = all_mine ? 1u : 0u;\n"
        "    answer[2] = all_true ? 1u : 0u;\n"
        "    answer[3] = all_same ? 1u : 0u;\n"
        "  }\n"
        "}\n";
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(shader, 1, &cs_src, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    GLuint out = 0;
    glGenBuffers(1, &out);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    const GLuint filler[4] = {0xdeadu, 0xdeadu, 0xdeadu, 0xdeadu};
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(filler), filler,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, out);
    glUseProgram(program);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    GLuint got[4] = {0, 0, 0, 0};
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO votes any=%u all_mine=%u all_true=%u all_same=%u "
            "err=0x%04x\n", got[0], got[1], got[2], got[3], (unsigned)error);

    result("any_true", error == GL_NO_ERROR && got[0] == 1);
    result("all_uniform", error == GL_NO_ERROR && got[2] == 1);
    /* Both of these are false only because the invocations disagree, which is
     * what a ballot over a group of one could not report. */
    result("all_mixed", error == GL_NO_ERROR && got[1] == 0 && got[3] == 0);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
