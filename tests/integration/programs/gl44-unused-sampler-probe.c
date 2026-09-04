/* A sampler the shader declares, does not execute, and has no matching
 * texture for.
 *
 * GL calls a sampler whose bound texture does not match its type *incomplete*
 * and says it reads (0,0,0,1) -- not that the draw is an error. WebGPU has no
 * such notion: every declared binding must match the layout and every layout
 * entry must name a real view, so this driver declined the whole draw and the
 * frame lost geometry that has nothing to do with the sampler in question.
 *
 * The shape is ordinary. An uber-shader picking between two samplers at run
 * time is how most engines handle a material with and without a map:
 *
 *     if (u_pick > 0) colour = texture(u_integer, uv);
 *     else            colour = texture(u_float, uv);
 *
 * Only one branch runs, and only one texture need be worth anything -- but
 * both samplers are declared, and the unused one's unit may hold whatever was
 * left there.
 *
 *   `unused_mismatched`  the float branch runs while the integer sampler's
 *                        unit holds an ordinary unorm texture
 *   `used_still_works`   the same program with the integer branch selected
 *                        and a real integer texture bound, so that the fix
 *                        cannot be "ignore the second sampler always"
 *
 * KHR-GL45.sample_variables' verification shader is exactly the first case --
 * it declares `${SAMPLER}` and `${SAMPLER}MS` and picks on a uniform -- and it
 * cost that family its whole integer and float half.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 16

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
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3,
        EGL_CONTEXT_OPENGL_PROFILE_MASK,
        EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

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

    /* A green unorm texture and a red-ish integer one. */
    const unsigned char green[4] = {0, 200, 0, 255};
    GLuint tex_float = 0;
    glGenTextures(1, &tex_float);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex_float);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, green);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    const int texels[4] = {40, 0, 0, 1};
    GLuint tex_int = 0;
    glGenTextures(1, &tex_int);
    glActiveTexture(GL_TEXTURE2);
    glBindTexture(GL_TEXTURE_2D, tex_int);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32I, 1, 1, 0, GL_RGBA_INTEGER,
                 GL_INT, texels);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 3.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 3.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "uniform sampler2D u_float;\n"
        "uniform isampler2D u_integer;\n"
        "uniform int u_pick;\n"
        "out vec4 colour;\n"
        "void main() {\n"
        "  if (u_pick > 0)\n"
        "    colour = vec4(texture(u_integer, vec2(0.5))) / 255.0;\n"
        "  else\n"
        "    colour = texture(u_float, vec2(0.5));\n"
        "}\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint linked = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        result("unused_mismatched", 0);
        result("used_still_works", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }
    glUseProgram(prog);

    /* The integer sampler points at unit 1, where the *unorm* texture is
     * bound -- incomplete, and never sampled because u_pick is zero. */
    glActiveTexture(GL_TEXTURE1);
    glBindTexture(GL_TEXTURE_2D, tex_float);
    glUniform1i(glGetUniformLocation(prog, "u_float"), 0);
    glUniform1i(glGetUniformLocation(prog, "u_integer"), 1);
    glUniform1i(glGetUniformLocation(prog, "u_pick"), 0);
    glClearColor(1.0f, 0.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    GLenum error = glGetError();
    fprintf(stderr, "INFO unused_mismatched got=%u,%u,%u want=0,200,0 "
            "err=0x%04x\n", px[0], px[1], px[2], (unsigned)error);
    result("unused_mismatched", error == GL_NO_ERROR && px[1] > 160 &&
                                px[0] < 60 && px[2] < 60);

    /* Now take the integer branch with a real integer texture, so that
     * "always ignore the second sampler" would fail here. */
    glUniform1i(glGetUniformLocation(prog, "u_integer"), 2);
    glUniform1i(glGetUniformLocation(prog, "u_pick"), 1);
    glClearColor(1.0f, 0.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    error = glGetError();
    fprintf(stderr, "INFO used_still_works got=%u,%u,%u want=40,0,0 "
            "err=0x%04x\n", px[0], px[1], px[2], (unsigned)error);
    result("used_still_works", error == GL_NO_ERROR && px[0] > 20 &&
                               px[0] < 60 && px[1] < 20);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
