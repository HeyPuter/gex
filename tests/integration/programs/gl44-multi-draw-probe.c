#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLuint compile(GLenum stage, const char *source) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[2048];
        glGetShaderInfoLog(shader, sizeof(log), NULL, log);
        fprintf(stderr, "shader compilation failed: %s\n", log);
        exit(2);
    }
    return shader;
}

static int near_pixel(const unsigned char *p, int r, int g, int b) {
    return abs((int)p[0] - r) <= 3 && abs((int)p[1] - g) <= 3 &&
           abs((int)p[2] - b) <= 3 && p[3] >= 252;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "FATAL egl_initialize error=0x%x\n", eglGetError());
        return 2;
    }
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count) ||
        config_count < 1) return 2;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 4, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 16, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    static const char *vs_source =
        "#version 330 core\n"
        "layout(location=0) in vec2 position;\n"
        "layout(location=1) in vec3 color;\n"
        "out vec3 varying_color;\n"
        "void main(){ gl_Position=vec4(position,0,1); varying_color=color; }\n";
    static const char *fs_source =
        "#version 330 core\n"
        "in vec3 varying_color;\n"
        "layout(location=0) out vec4 output_color;\n"
        "void main(){ output_color=vec4(varying_color,1); }\n";
    GLuint vs = compile(GL_VERTEX_SHADER, vs_source);
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_source);
    GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    result("program_link", linked == GL_TRUE);
    if (!linked) return 2;

    const GLfloat vertices[] = {
        -0.9f, -0.8f, 1, 0, 0,  -0.1f, -0.8f, 1, 0, 0,  -0.5f, 0.8f, 1, 0, 0,
         0.1f, -0.8f, 0, 1, 0,   0.9f, -0.8f, 0, 1, 0,   0.5f, 0.8f, 0, 1, 0,
    };
    GLuint vao = 0, vbo = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices, GL_STATIC_DRAW);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 5 * sizeof(GLfloat), (void *)0);
    glVertexAttribPointer(1, 3, GL_FLOAT, GL_FALSE, 5 * sizeof(GLfloat),
                          (void *)(2 * sizeof(GLfloat)));
    glEnableVertexAttribArray(0);
    glEnableVertexAttribArray(1);
    glUseProgram(program);
    glViewport(0, 0, 16, 8);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);

    const GLint first[] = {0, 3};
    const GLsizei count[] = {3, 3};
    glMultiDrawArrays(GL_TRIANGLES, first, count, 2);
    unsigned char left[4] = {0}, right[4] = {0};
    glReadPixels(4, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, left);
    glReadPixels(11, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, right);
    printf("INFO pixels left=%u,%u,%u,%u right=%u,%u,%u,%u\n",
           left[0], left[1], left[2], left[3], right[0], right[1], right[2], right[3]);
    result("multi_draw_pixels", near_pixel(left, 255, 0, 0) && near_pixel(right, 0, 255, 0));
    result("multi_draw_no_error", glGetError() == GL_NO_ERROR);

    glMultiDrawArrays(GL_TRIANGLES, first, count, -1);
    GLenum negative_error = glGetError();
    printf("INFO negative_drawcount_error=0x%x\n", negative_error);
    result("negative_drawcount", negative_error == GL_INVALID_VALUE);
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, cases - passed);
    return passed == cases ? 0 : 1;
}
