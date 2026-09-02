/* ARB_sample_shading: is the fragment shader really run once per sample?
 *
 * WebGPU has no pipeline switch for the shading rate -- it follows from the
 * shader, which is invoked per sample exactly when it reads
 * @builtin(sample_index). GL asks for the rate instead, with
 * glMinSampleShading, so the two only agree if gl_SampleID actually reaches
 * WGSL as sample_index. Advertising the extension without that is worse than
 * not advertising it, so this checks the arithmetic rather than the flag.
 *
 * Four samples writing gl_SampleID/3 resolve to 0.5 when each is shaded
 * separately, and to whatever the single invocation wrote when they are not.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

static const char *fragment_source =
    "#version 330 core\n"
    "#extension GL_ARB_sample_shading : require\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(float(gl_SampleID) / 3.0, 0.0, 0.0, 1.0); }\n";

static GLuint compile(GLenum stage, const char *source)
{
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile failed: %s\n", log);
        return 0;
    }
    return shader;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "FAIL egl init error=0x%04x\n", eglGetError());
        return 2;
    }
    EGLConfig config = NULL;
    EGLint count = 0;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE
    };
    eglChooseConfig(display, config_attributes, &config, 1, &count);
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4,
        EGL_CONTEXT_MINOR_VERSION, 4,
        EGL_NONE
    };
    const EGLint surface_attributes[] = {
        EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE
    };
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (count < 1 || context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) {
        return 2;
    }

    if (!eglBindAPI(EGL_OPENGL_API)) return 2;

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) { result("compile", 0); goto done; }
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link failed: %s\n", log);
        result("link", 0);
        goto done;
    }
    result("compile", 1);
    result("link", 1);

    GLuint ms = 0, ms_fbo = 0;
    glGenTextures(1, &ms);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms);
    glTexStorage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, 4, GL_RGBA8, 16, 16,
                              GL_TRUE);
    glGenFramebuffers(1, &ms_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, ms_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D_MULTISAMPLE, ms, 0);
    result("multisample_fbo",
           glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);

    GLuint resolved = 0, resolve_fbo = 0;
    glGenTextures(1, &resolved);
    glBindTexture(GL_TEXTURE_2D, resolved);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 16, 16);
    glGenFramebuffers(1, &resolve_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, resolve_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           resolved, 0);

    glBindFramebuffer(GL_FRAMEBUFFER, ms_fbo);
    glUseProgram(program);
    glEnable(GL_SAMPLE_SHADING);
    glMinSampleShading(1.0f);

    GLuint vao = 0, buffer = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    static const float quad[8] = { -1, -1, 1, -1, -1, 1, 1, 1 };
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glViewport(0, 0, 16, 16);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

    glBindFramebuffer(GL_READ_FRAMEBUFFER, ms_fbo);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, resolve_fbo);
    glBlitFramebuffer(0, 0, 16, 16, 0, 0, 16, 16, GL_COLOR_BUFFER_BIT,
                      GL_NEAREST);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, resolve_fbo);

    unsigned char pixel[4] = { 0, 0, 0, 0 };
    glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO resolved red=%u error=0x%04x\n", pixel[0],
            (unsigned)error);
    /* Per sample: 0, 1/3, 2/3, 1 average to 0.5 -- 128 of 255. Per fragment:
     * every sample takes the same value and the resolve is that value. The
     * window is wide enough for the resolve's rounding and nothing else. */
    result("per_sample_shading",
           error == GL_NO_ERROR && pixel[0] > 110 && pixel[0] < 146);

done:
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
