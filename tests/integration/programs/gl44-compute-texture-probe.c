/* A compute shader that *samples* a texture.
 *
 * GL 4.3's compute shaders sample as freely as a fragment shader does, and a
 * renderer that does its post-processing in compute does little else. This
 * driver had no support for it at all: the pass that splits a combined
 * `sampler2D` into the texture and sampler WebGPU wants was run only for the
 * graphics stages, so a compute shader arrived at the SPIR-V emitter with its
 * texture derefs already lowered away by st/mesa and asserted there --
 *
 *   nir_to_spirv.c:4702: emit_tex: Assertion `var' failed.
 *
 * -- which aborts the *application*, not the shader. Blender's EEVEE reaches
 * it. Binding a view for the compute stage was refused separately, which
 * latched the context.
 *
 * So the three cases are the three things that have to work end to end: a
 * filtered sample, a texelFetch (no sampler involved, which is a different
 * WGSL binding shape), and a 2D *array* sample, which is where the view
 * dimension the bind group layout names has to match the shader's.
 *
 *   `sample`  texture() through a sampler
 *   `fetch`   texelFetch() with an explicit level
 *   `array`   texture() on a sampler2DArray, reading layer 1
 *
 * Every texel is a distinct value, so a driver that binds the wrong slot,
 * the wrong layer or the wrong level reads back the wrong number rather than
 * something plausible.
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

static GLuint build_compute(const char *source) {
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        return 0;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return program;
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

    /* A 2x2 texture, one distinct red value per texel. */
    const unsigned char texels[4][4] = {
        { 10, 0, 0, 255}, { 40, 0, 0, 255},   /* row 0: (0,0) (1,0) */
        { 70, 0, 0, 255}, {100, 0, 0, 255},   /* row 1: (0,1) (1,1) */
    };
    GLuint flat = 0;
    glGenTextures(1, &flat);
    glBindTexture(GL_TEXTURE_2D, flat);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 2, 2, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, texels);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

    /* Two layers of 1x1, so the layer index is the only thing that can be
     * read wrong. */
    const unsigned char layers[2][4] = {{60, 0, 0, 255}, {90, 0, 0, 255}};
    GLuint array_tex = 0;
    glGenTextures(1, &array_tex);
    glBindTexture(GL_TEXTURE_2D_ARRAY, array_tex);
    glTexImage3D(GL_TEXTURE_2D_ARRAY, 0, GL_RGBA8, 1, 1, 2, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, layers);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    GLuint out_buffer = 0;
    glGenBuffers(1, &out_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
    {
        const GLuint filler[4] = {0xdeadu, 0xdeadu, 0xdeadu, 0xdeadu};
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(filler), filler,
                     GL_DYNAMIC_COPY);
    }
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, out_buffer);

    if (glGetError() != GL_NO_ERROR) {
        fprintf(stderr, "INFO setup failed\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 2;
    }

    /* The sample and the fetch share a dispatch: both read the same texture,
     * which is also the case a driver binding one slot for two uses gets
     * wrong. */
    GLuint flat_program = build_compute(
        "#version 430\n"
        "layout(local_size_x = 1) in;\n"
        "layout(binding = 0) uniform sampler2D src;\n"
        "layout(std430, binding = 0) buffer Out { uint value[]; };\n"
        "void main() {\n"
        "  value[0] = uint(texture(src, vec2(0.25, 0.25)).r * 255.0 + 0.5);\n"
        "  value[1] = uint(texelFetch(src, ivec2(1, 0), 0).r * 255.0 + 0.5);\n"
        "}\n");
    if (flat_program) {
        glUseProgram(flat_program);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, flat);
        glUniform1i(glGetUniformLocation(flat_program, "src"), 0);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    }
    GLuint got[4] = {0, 0, 0, 0};
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
    GLenum error = glGetError();
    fprintf(stderr, "INFO sample got=%u want=10 err=0x%04x\n", got[0],
            (unsigned)error);
    result("sample", flat_program && error == GL_NO_ERROR && got[0] == 10);
    fprintf(stderr, "INFO fetch got=%u want=40\n", got[1]);
    result("fetch", flat_program && error == GL_NO_ERROR && got[1] == 40);

    /* The array case, whose view dimension the layout has to name as 2DArray
     * or the bind group is rejected. */
    GLuint array_program = build_compute(
        "#version 430\n"
        "layout(local_size_x = 1) in;\n"
        "layout(binding = 0) uniform sampler2DArray src;\n"
        "layout(std430, binding = 0) buffer Out { uint value[]; };\n"
        "void main() {\n"
        "  value[2] = uint(texture(src, vec3(0.5, 0.5, 1.0)).r * 255.0 + 0.5);\n"
        "}\n");
    if (array_program) {
        glUseProgram(array_program);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D_ARRAY, array_tex);
        glUniform1i(glGetUniformLocation(array_program, "src"), 0);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    }
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out_buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), got);
    error = glGetError();
    fprintf(stderr, "INFO array got=%u want=90 err=0x%04x\n", got[2],
            (unsigned)error);
    result("array", array_program && error == GL_NO_ERROR && got[2] == 90);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
