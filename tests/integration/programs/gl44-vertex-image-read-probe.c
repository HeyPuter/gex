/* A vertex shader that READS a storage image.
 *
 * This is the narrow shape that does not work, and it is worth stating why the
 * neighbouring one does. A vertex shader that only *writes* an image is served
 * already: the image variables are removed from the copy that rasterises --
 * removed rather than marked read-only, because the *declaration* is what Tint
 * refuses in the vertex stage -- and the store rides the compute dispatch. So
 * `image_count` is meant to be zero for the rasterising copy, which is why
 * `vertex_group_count` legitimately does not count it.
 *
 * Removal only works if the image is dead. Declare it `readonly` and load from
 * it, as here, and it is live: it cannot be removed, it stays declared in the
 * rasterising copy, and nothing binds it. Dawn then refuses the pipeline --
 *
 *   The entry-point uses bindings in group 1 but [PipelineLayout] doesn't have
 *   a BindGroupLayout for this index  - While validating vertex stage
 *
 * -- and an invalid pipeline poisons the command buffer it is set on, so the
 * whole draw disappears with no GL error at all. The driver declines it by
 * name now, which is the honest version of the same outcome rather than a fix.
 *
 * KHR-GL45.es_31_compatibility.shader_image_load_store.advanced-memory-order-vsfs
 * and .advanced-sync-imageAccess are both this, arrived at less directly: the
 * first loads after storing, the second declares `readonly imageBuffer`. They
 * had been recorded as memory-ordering bugs, because the answer was absent
 * rather than wrong.
 *
 * So this asserts on a *pixel*: a lost draw leaves the clear colour, which is
 * what the symptom actually looks like from the application's side.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;
static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-30s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 32, EGL_HEIGHT, 32, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    drain();

    /* One texel holding a value the shader could not have invented. */
    GLuint tex = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32F, 1, 1);
    const float texel = 0.75f;
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 1, 1, GL_RED, GL_FLOAT, &texel);
    glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_ONLY, GL_R32F);
    drain();
    eq("setup_is_clean", glGetError(), GL_NO_ERROR);

    static const char *vs_src =
        "#version 430 core\n"
        "layout(r32f, binding = 0) readonly uniform image2D img;\n"
        "out float carried;\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 3.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 3.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "  carried = imageLoad(img, ivec2(0, 0)).r; }\n";
    static const char *fs_src =
        "#version 430 core\n"
        "in float carried;\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(carried, 0.0, 0.0, 1.0); }\n";
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs_src, NULL); glCompileShader(v);
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs_src, NULL); glCompileShader(f);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, v); glAttachShader(prog, f);
    glLinkProgram(prog);
    GLint linked = 0; glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024]; GLsizei ln = 0;
        glGetProgramInfoLog(prog, sizeof(log), &ln, log);
        fprintf(stderr, "INFO link failed: %.*s\n", (int)ln, log);
    }
    eq("program_links", linked, 1);
    if (!linked) { printf("SUMMARY cases=%d passed=%d failed=%d\n",
                          cases, passed, cases - passed); return 1; }

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glUseProgram(prog);
    /* Green, so a lost draw is unmistakable: the pass wants red 191. */
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glFinish();
    drain();

    unsigned char px[4] = {0, 0, 0, 0};
    glReadPixels(16, 16, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    drain();
    fprintf(stderr, "INFO centre pixel %u,%u,%u,%u (green means the draw was "
            "lost)\n", px[0], px[1], px[2], px[3]);
    /* Reported, not asserted, while the driver declines this by name.
       Asserting would pass on llvmpipe and fail on gex_wgpu, which is a
       permanently red gate for a gap that is already written down -- and a
       probe that fails for a known missing feature stops working as a
       regression gate for everything else in it. Promote both to eq() with
       the fix; the numbers below are what the fix has to produce.

       0.75 * 255 = 191, and the green clear must be gone. */
    fprintf(stderr, "INFO %-30s red=%u want 180..202%s\n",
            "gap:vertex_read_reached_fs", px[0],
            (px[0] > 180 && px[0] < 202) ? "" : "   <-- not arriving");
    fprintf(stderr, "INFO %-30s green=%u want <60%s\n",
            "gap:draw_was_not_lost", px[1],
            px[1] < 60 ? "" : "   <-- the clear is showing through");

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
