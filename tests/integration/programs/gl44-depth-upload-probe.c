/* Uploading depth data, whole and in pieces.
 *
 * WebGPU takes no buffer copy into the depth aspect of `depth32float`, so
 * this driver uploads depth by *rendering* it: the floats go into an r32float
 * staging texture, and a fullscreen draw moves them into the depth attachment
 * through `@builtin(frag_depth)`, with the viewport limiting the draw to the
 * box the caller named. That is a lot of machinery for something an
 * application does with one call, and it had a bug that only a partial upload
 * could show: the fragment shader indexed the staging texture by
 * `@builtin(position)`, which is the *framebuffer* coordinate, while the
 * staging texture holds only the box. Every upload away from the origin read
 * out of range -- which WGSL's textureLoad answers with a zero rather than an
 * error -- and so wrote a depth of **zero over exactly the region it was
 * filling**.
 *
 * That is a nasty shape: `glTexImage2D` with data was correct, so a texture
 * uploaded in one go worked and one built up in pieces did not, and the
 * damage was confined to the piece, which is the part a test then reads.
 *
 *   `whole_level`   data handed to glTexImage2D, the path that always worked
 *                   and the control for the others
 *   `subimage_full` glTexSubImage2D covering the whole level
 *   `subimage_at`   a 2x2 patch at a non-zero offset, read back at the patch
 *   `neighbours`    a texel *outside* that patch still holds what it held --
 *                   the check that a fix which uploads the whole level, or
 *                   clears before drawing, would fail
 *
 * The values are distinct per texel, so a driver that writes the patch to the
 * wrong place fails `subimage_at` and `neighbours` differently and the two
 * together say where it went.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 32

/* Read one texel of the depth texture as a float, through a plain sampler --
 * the comparison is off, so this is the stored value and not a verdict. */
static GLuint build(void) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "uniform sampler2D src;\n"
        "uniform ivec2 at;\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(texelFetch(src, at, 0).r); }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLint ok = 0;
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO fs compile: %s\n", log);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return prog;
}

static GLuint program;

static float read_texel(GLuint tex, int x, int y) {
    const float clear[4] = {-1, -1, -1, -1};
    glClearBufferfv(GL_COLOR, 0, clear);
    glUseProgram(program);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex);
    glUniform1i(glGetUniformLocation(program, "src"), 0);
    glUniform2i(glGetUniformLocation(program, "at"), x, y);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    float px[4] = {0, 0, 0, 0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_FLOAT, px);
    return px[0];
}

/* A depth texture with a full mip chain, which is what GL's default
 * NEAREST_MIPMAP_LINEAR minification makes a texture need to be complete --
 * an incomplete texture samples as zero and would fail every case here for
 * the wrong reason. */
static GLuint make_depth(int with_data, const float *level0) {
    GLuint tex = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    for (int i = 0, size = SIDE; size > 0; ++i, size /= 2)
        glTexImage2D(GL_TEXTURE_2D, i, GL_DEPTH_COMPONENT32F, size, size, 0,
                     GL_DEPTH_COMPONENT, GL_FLOAT,
                     (i == 0 && with_data) ? level0 : NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    return tex;
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

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32F, 1, 1, 0, GL_RGBA, GL_FLOAT,
                 NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, 1, 1);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    program = build();
    if (!program) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    /* A distinct value per texel, so a misplaced write is identifiable. */
    float *image = malloc((size_t)SIDE * SIDE * sizeof(float));
    for (int y = 0; y < SIDE; ++y)
        for (int x = 0; x < SIDE; ++x)
            image[y * SIDE + x] = (float)(y * SIDE + x) / (float)(SIDE * SIDE);
    const float patch[4] = {12.f/16, 13.f/16, 14.f/16, 15.f/16};
    const int px = 22, py = 25;

    /* The control: the whole level, handed over at creation. */
    GLuint whole = make_depth(1, image);
    float got = read_texel(whole, px, py);
    float want = image[py * SIDE + px];
    fprintf(stderr, "INFO whole_level got=%.5f want=%.5f err=0x%04x\n", got,
            want, (unsigned)glGetError());
    result("whole_level", fabsf(got - want) < 0.001f);

    /* The whole level again, as one subimage. */
    GLuint full = make_depth(0, NULL);
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, SIDE, SIDE, GL_DEPTH_COMPONENT,
                    GL_FLOAT, image);
    got = read_texel(full, px, py);
    fprintf(stderr, "INFO subimage_full got=%.5f want=%.5f err=0x%04x\n", got,
            want, (unsigned)glGetError());
    result("subimage_full", fabsf(got - want) < 0.001f);

    /* A 2x2 patch at an offset, over the image above. */
    GLuint part = make_depth(0, NULL);
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, SIDE, SIDE, GL_DEPTH_COMPONENT,
                    GL_FLOAT, image);
    glTexSubImage2D(GL_TEXTURE_2D, 0, px, py, 2, 2, GL_DEPTH_COMPONENT,
                    GL_FLOAT, patch);
    got = read_texel(part, px, py);
    fprintf(stderr, "INFO subimage_at got=%.5f want=%.5f err=0x%04x\n", got,
            patch[0], (unsigned)glGetError());
    result("subimage_at", fabsf(got - patch[0]) < 0.001f);

    /* And a texel the patch did not cover, which must be untouched. */
    const int nx = px - 3, ny = py - 3;
    got = read_texel(part, nx, ny);
    want = image[ny * SIDE + nx];
    fprintf(stderr, "INFO neighbours at %d,%d got=%.5f want=%.5f\n", nx, ny,
            got, want);
    result("neighbours", fabsf(got - want) < 0.001f);

    free(image);
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
