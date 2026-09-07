/* gl_Layer written from a non-constant expression.
 *
 * A geometry shader that computes its layer draws **nothing at all** here --
 * not into the wrong layer, nothing -- while the same shader with a constant
 * gl_Layer is correct. No GL error, no driver message, no failed link.
 *
 * Computing the layer is the ordinary case: a cube shadow map, a cascade
 * array and a single-pass cube render all derive it from a loop counter, so
 * this is a live defect rather than a corner. It is what
 * KHR-GL44.geometry_shader's three layered_rendering cases fail on.
 *
 * The isolation is the point, and it narrowed twice. `max_vertices` is
 * irrelevant -- 18 with a constant layer is correct and 3 with a computed one
 * is not -- and so is being non-constant: `gl_Layer = n - 1` from a *uniform*
 * works. What breaks it is a layer that **differs between the primitives one
 * invocation emits**, which is the whole point of layered rendering: a
 * single-pass cube render emits six primitives with six layers.
 *
 *   no gl_Layer, looped                  -> correct
 *   gl_Layer = 0        (literal)        -> correct
 *   gl_Layer = n - 1    (uniform)        -> correct
 *   gl_Layer = l        (loop counter)   -> nothing drawn
 *
 * A uniform is the same for every primitive, so it can be hoisted; a loop
 * counter cannot. All four are kept so a fix cannot appear to work by moving
 * the trigger onto one of the cases that already passed.
 *
 * Every case renders into a *plain 2D* framebuffer with a single layer, where
 * GL says every primitive lands on the one layer whatever gl_Layer holds. That
 * removes layered attachments, the driver's per-layer routing and the read-back
 * path from the question entirely: what is left is the shader.
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
    fprintf(stderr, "INFO %-34s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 400\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
static const char *fs_src =
    "#version 400\n"
    "flat in int which;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(float(which) / 255.0, 0.0, 0.0, 1.0); }\n";

/* Bodies differ only where the comment says. Each emits one full-viewport
   triangle with `which` = 1, so a correct run reads red = 1 in every case. */
#define HEAD(maxv) \
    "#version 400\n" \
    "layout(points) in;\n" \
    "layout(triangle_strip, max_vertices = " #maxv ") out;\n"
#define TRI(setup) \
    setup "gl_Position = vec4(-1.0, -1.0, 0.0, 1.0); EmitVertex();\n" \
    setup "gl_Position = vec4( 3.0, -1.0, 0.0, 1.0); EmitVertex();\n" \
    setup "gl_Position = vec4(-1.0,  3.0, 0.0, 1.0); EmitVertex();\n" \
    "EndPrimitive();\n"

/* No gl_Layer at all: the control. */
static const char *gs_none =
    HEAD(18) "uniform int n;\nflat out int which;\nvoid main() {\n"
    "  for (int l = 0; l < n; ++l) {\n" TRI("which = l + 1; ") "  }\n}\n";
/* gl_Layer from a literal. */
static const char *gs_constant =
    HEAD(18) "flat out int which;\nvoid main() {\n"
    TRI("gl_Layer = 0; which = 1; ") "}\n";
/* gl_Layer from a loop counter -- the only difference that matters. */
static const char *gs_computed =
    HEAD(3) "uniform int n;\nflat out int which;\nvoid main() {\n"
    "  for (int l = 0; l < n; ++l) {\n"
    TRI("gl_Layer = l; which = l + 1; ") "  }\n}\n";
/* From a uniform: non-constant, but the same for every primitive. This one
   passes, which is what narrows the trigger to "differs per primitive". */
static const char *gs_uniform_layer =
    HEAD(3) "uniform int n;\nflat out int which;\nvoid main() {\n"
    TRI("gl_Layer = n - 1; which = 1; ") "}\n";

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1200] = {0};
               glGetShaderInfoLog(sh, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s compile failed: %s\n", what, log); }
    return ok ? sh : 0;
}

static int red_from(const char *label, const char *gs_src) {
    GLuint vs = compile(GL_VERTEX_SHADER, vs_src, "vs");
    GLuint gs = compile(GL_GEOMETRY_SHADER, gs_src, label);
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !gs || !fs) return -2;
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, gs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1200] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO %s link failed: %s\n", label, log);
                   return -1; }
    GLuint tex = 0; glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 8, 8);
    GLuint fbo = 0; glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           tex, 0);
    glViewport(0, 0, 8, 8);
    glUseProgram(program);
    const GLint n = glGetUniformLocation(program, "n");
    if (n >= 0) glUniform1i(n, 1);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_POINTS, 0, 1);
    unsigned char pixel[4] = {0};
    glReadPixels(4, 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    while (glGetError() != GL_NO_ERROR) { }
    glDeleteFramebuffers(1, &fbo);
    glDeleteTextures(1, &tex);
    glDeleteProgram(program);
    return pixel[0];
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
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);

    eq("no_layer_write_draws", red_from("no layer", gs_none), 1);
    eq("constant_layer_draws", red_from("constant", gs_constant), 1);
    eq("computed_layer_draws", red_from("computed", gs_computed), 1);
    eq("uniform_layer_draws", red_from("uniform", gs_uniform_layer), 1);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
