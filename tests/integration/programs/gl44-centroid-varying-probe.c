/* A `centroid` varying must not cost the draw.
 *
 * GL puts the interpolation qualifier on the *fragment input* alone: a vertex
 * shader that says nothing about a varying its fragment shader declares
 * `centroid in` is ordinary, correct GL. WebGPU is stricter -- it requires
 * both stages to declare the same interpolation *sampling* and rejects the
 * pipeline otherwise -- and an invalid render pipeline invalidates the whole
 * command buffer, so the draw and everything batched with it was lost.
 *
 * The obvious fix is to copy the qualifier onto the vertex output, and it
 * cannot be done: SpvDecorationCentroid and SpvDecorationSample are defined
 * for fragment inputs, a pre-fragment output carrying them is not valid
 * SPIR-V, and Tint drops them. The driver emitted them there for a while and
 * produced a module byte-identical to the one it was meant to differ from.
 * So the fragment side gives the qualifier up instead and the varying is
 * interpolated at the pixel centre.
 *
 * That is a real difference -- centroid and centre differ where a pixel is
 * partially covered -- but GL already leaves that to the implementation, and
 * it is a difference rather than a lost frame.
 *
 *   `centroid_draws`  a `centroid in` varying renders at all
 *   `centroid_value`  and carries its value, rather than rendering something
 *                     the interpolation was dropped from entirely
 *   `sample_draws`    the same for `sample in`, which is the qualifier the
 *                     driver cannot honour and must still not choke on
 *
 * Every case writes a constant marker alongside the value: a draw that never
 * happened and a value that came back zero are indistinguishable otherwise,
 * and while this was being written they were both true at different moments.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <math.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 8

/* `vs_qualifier` is what the *vertex* shader declares. GLSL lets `centroid`
 * appear on the fragment input alone -- that asymmetry is the case this
 * driver had to be taught -- but requires `sample` on both sides, so a
 * sample-qualified varying is always declared twice and the probe has to do
 * the same or it is testing a GLSL error rather than a driver. */
static GLuint build(const char *qualifier, const char *vs_qualifier) {
    char vs_src[512], fs_src[512];
    snprintf(vs_src, sizeof(vs_src),
             "#version 400 core\n"
             "layout(location = 0) in vec2 at;\n"
             "%s out vec4 v_value;\n"
             "void main() {\n"
             "  gl_Position = vec4(at, 0.0, 1.0);\n"
             "  v_value = vec4(at.x * 0.5 + 0.5, 0.0, 0.0, 1.0);\n"
             "}\n", vs_qualifier);
    snprintf(fs_src, sizeof(fs_src),
             "#version 400 core\n"
             "%s in vec4 v_value;\n"
             "out vec4 colour;\n"
             /* Blue is the marker: it is written whatever the interpolation
              * does, so an untouched target is not read as a zero value. */
             "void main() { colour = vec4(v_value.x, 0.0, 0.75, 1.0); }\n",
             qualifier);
    const char *vs_ptr = vs_src, *fs_ptr = fs_src;
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_ptr, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_ptr, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[600] = {0};
        glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s fragment compile: %s\n", qualifier, log);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[600] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s link: %s\n", qualifier, log);
        return 0;
    }
    return prog;
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
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint colour = 0, fbo = 0, vao = 0, buffer = 0;
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
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 2 * sizeof(float), NULL);
    const float full[6] = {-1.0f, -1.0f,  3.0f, -1.0f,  -1.0f, 3.0f};
    glBufferData(GL_ARRAY_BUFFER, sizeof(full), full, GL_STREAM_DRAW);

    struct { const char *qualifier; const char *vs; const char *name; }
    forms[] = {
        /* Fragment only: the asymmetry GL allows and WebGPU does not. */
        {"centroid", "", "centroid"},
        /* Both sides, because GLSL requires it of `sample`. */
        {"sample", "sample", "sample"},
    };
    for (unsigned i = 0; i < 2; ++i) {
        GLuint program = build(forms[i].qualifier, forms[i].vs);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        if (program) {
            glUseProgram(program);
            glDrawArrays(GL_TRIANGLES, 0, 3);
        }
        unsigned char px[4] = {0};
        /* Pixel 6 of 8 across: clip x = 0.5, so the varying is 0.75 there. */
        glReadPixels(6, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        const int drew = px[2] > 128;      /* the marker */
        const int value = px[0] > 160 && px[0] < 224;   /* about 0.75 */
        fprintf(stderr, "INFO %s got=%u,%u,%u marker=%d value=%d err=0x%04x\n",
                forms[i].name, px[0], px[1], px[2], drew, value,
                (unsigned)error);
        char label[32];
        snprintf(label, sizeof(label), "%s_draws", forms[i].name);
        result(label, program && error == GL_NO_ERROR && drew);
        if (i == 0)
            result("centroid_value", program && error == GL_NO_ERROR && value);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
