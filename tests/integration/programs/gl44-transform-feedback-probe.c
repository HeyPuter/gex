#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static GLuint shader(GLenum type, const char *source) {
    GLuint value = glCreateShader(type);
    glShaderSource(value, 1, &source, NULL);
    glCompileShader(value);
    GLint ok = 0;
    glGetShaderiv(value, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[2048] = {};
        glGetShaderInfoLog(value, sizeof(log), NULL, log);
        fprintf(stderr, "FATAL compile: %s\n", log);
        return 0;
    }
    return value;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attribs[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE
    };
    EGLConfig config = NULL;
    EGLint count = 0;
    if (!eglChooseConfig(display, config_attribs, &config, 1, &count) || count < 1) return 3;
    const EGLint surface_attribs[] = {EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attribs);
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    if (surface == EGL_NO_SURFACE || context == EGL_NO_CONTEXT ||
        !eglMakeCurrent(display, surface, surface, context)) return 4;

    const char *vs_source =
        "#version 440 core\n"
        "flat out int max_value;\n"
        "flat out int marker;\n"
        "void main(){ max_value=gl_MaxClipDistances; marker=0x12345678; "
        "gl_Position=vec4(0,0,0,1); }\n";
    const char *fs_source =
        "#version 440 core\n"
        "out vec4 color; void main(){ color=vec4(0); }\n";
    GLuint vs = shader(GL_VERTEX_SHADER, vs_source);
    GLuint fs = shader(GL_FRAGMENT_SHADER, fs_source);
    if (!vs || !fs) return 5;
    GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    glAttachShader(program, fs);
    const char *varyings[] = {"max_value", "marker"};
    glTransformFeedbackVaryings(program, 2, varyings, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[2048] = {};
        glGetProgramInfoLog(program, sizeof(log), NULL, log);
        fprintf(stderr, "FATAL link: %s\n", log);
        return 6;
    }

    GLuint vao = 0, buffer = 0;
    const GLint initial[] = {0x11111111, 0x22222222};
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, buffer);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, sizeof(initial), initial, GL_STATIC_READ);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, buffer);
    glUseProgram(program);
    glEnable(GL_RASTERIZER_DISCARD);
    glBeginTransformFeedback(GL_POINTS);
    glDrawArrays(GL_POINTS, 0, 1);
    glEndTransformFeedback();
    glDisable(GL_RASTERIZER_DISCARD);

    GLint direct[2] = {};
    glGetBufferSubData(GL_TRANSFORM_FEEDBACK_BUFFER, 0, sizeof(direct), direct);
    GLint mapped[2] = {};
    const GLint *map = glMapBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, GL_READ_ONLY);
    if (map) memcpy(mapped, map, sizeof(mapped));
    if (map) glUnmapBuffer(GL_TRANSFORM_FEEDBACK_BUFFER);
    const GLenum error = glGetError();
    printf("TF direct=%d,0x%08x map=%d,0x%08x error=0x%x\n",
           direct[0], (unsigned)direct[1], mapped[0], (unsigned)mapped[1], error);
    const int passed = direct[0] == 8 && direct[1] == 0x12345678 &&
                       mapped[0] == 8 && mapped[1] == 0x12345678 && error == GL_NO_ERROR;
    printf("SUMMARY cases=1 passed=%d failed=%d\n", passed, !passed);
    return passed ? 0 : 1;
}
