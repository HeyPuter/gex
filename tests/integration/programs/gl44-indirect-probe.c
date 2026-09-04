#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "indirect draw failed: %s\n", message); return __LINE__; } \
} while (0)

struct draw_arrays_command {
    uint32_t count, instance_count, first, base_instance;
};

struct draw_elements_command {
    uint32_t count, instance_count, first_index;
    int32_t base_vertex;
    uint32_t base_instance;
};

static GLuint compile_shader(GLenum type, const char* source) {
    GLuint shader = glCreateShader(type);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[2048] = {0};
        glGetShaderInfoLog(shader, sizeof(log), NULL, log);
        fprintf(stderr, "shader compile failed: %s\n", log);
        exit(2);
    }
    return shader;
}

static void check_green(const char* operation) {
    unsigned char pixel[4] = {0, 0, 0, 0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    GLenum error = glGetError();
    if (error != GL_NO_ERROR || pixel[0] > 2 || pixel[1] < 253 ||
        pixel[2] > 2 || pixel[3] < 253) {
        fprintf(stderr, "indirect draw failed: %s error=0x%x pixel=%u,%u,%u,%u\n",
                operation, error, pixel[0], pixel[1], pixel[2], pixel[3]);
        exit(3);
    }
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
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    EGLConfig config = NULL;
    EGLint config_count = 0;
    /* An explicit pbuffer config, and a checked config_count. With a NULL
     * attribute list eglChooseConfig defaults to EGL_WINDOW_BIT, which matches
     * nothing under EGL_PLATFORM=surfaceless: it returned zero configs, the
     * config stayed NULL, the pbuffer failed -- and eglMakeCurrent succeeded
     * anyway, leaving a context with no default framebuffer. */
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_DEPTH_SIZE, 24, EGL_STENCIL_SIZE, 8, EGL_NONE,
    };
    const EGLint gex_pbuffer_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    CHECK(eglChooseConfig(display, config_attributes, &config, 1, &config_count), "eglChooseConfig");
    CHECK(config_count >= 1 && config != NULL, "no pbuffer config");
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, gex_pbuffer_attributes);
    CHECK(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    const char* vertex_source =
        "#version 330 core\n"
        "layout(location=0) in vec2 position;\n"
        "void main() { gl_Position = vec4(position, 0.0, 1.0); }\n";
    const char* fragment_source =
        "#version 330 core\n"
        "out vec4 color;\n"
        "void main() { color = vec4(0.0, 1.0, 0.0, 1.0); }\n";
    GLuint vertex_shader = compile_shader(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment_shader = compile_shader(GL_FRAGMENT_SHADER, fragment_source);
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex_shader);
    glAttachShader(program, fragment_shader);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    CHECK(linked, "program link");
    glUseProgram(program);

    const float vertices[] = {-1.0f, -1.0f, 3.0f, -1.0f, -1.0f, 3.0f};
    const uint16_t indices[] = {0, 1, 2};
    GLuint vao = 0, vertex_buffer = 0, index_buffer = 0, indirect_buffer = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &vertex_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, vertex_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
    glGenBuffers(1, &index_buffer);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, index_buffer);
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(indices), indices, GL_STATIC_DRAW);
    glGenBuffers(1, &indirect_buffer);
    glBindBuffer(GL_DRAW_INDIRECT_BUFFER, indirect_buffer);
    glViewport(0, 0, 1, 1);

    const struct draw_arrays_command arrays_one = {3, 1, 0, 3};
    glBufferData(GL_DRAW_INDIRECT_BUFFER, sizeof(arrays_one), &arrays_one, GL_STATIC_DRAW);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArraysIndirect(GL_TRIANGLES, 0);
    check_green("glDrawArraysIndirect");

    const struct draw_arrays_command arrays_many[] = {
        {0, 1, 0, 0}, {3, 1, 0, 7},
    };
    glBufferData(GL_DRAW_INDIRECT_BUFFER, sizeof(arrays_many), arrays_many, GL_STATIC_DRAW);
    glClear(GL_COLOR_BUFFER_BIT);
    glMultiDrawArraysIndirect(GL_TRIANGLES, 0, 2, sizeof(arrays_many[0]));
    check_green("glMultiDrawArraysIndirect");

    const struct draw_elements_command elements_one = {3, 1, 0, 0, 5};
    glBufferData(GL_DRAW_INDIRECT_BUFFER, sizeof(elements_one), &elements_one, GL_STATIC_DRAW);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawElementsIndirect(GL_TRIANGLES, GL_UNSIGNED_SHORT, 0);
    check_green("glDrawElementsIndirect");

    const struct draw_elements_command elements_many[] = {
        {0, 1, 0, 0, 0}, {3, 1, 0, 0, 9},
    };
    glBufferData(GL_DRAW_INDIRECT_BUFFER, sizeof(elements_many), elements_many, GL_STATIC_DRAW);
    glClear(GL_COLOR_BUFFER_BIT);
    glMultiDrawElementsIndirect(GL_TRIANGLES, GL_UNSIGNED_SHORT, 0, 2,
                                sizeof(elements_many[0]));
    check_green("glMultiDrawElementsIndirect");

    /* ARB_indirect_parameters: the *number of draws* comes from a buffer
     * too. The commands below hold three, and the count buffer says two --
     * so a driver ignoring the count draws the third, whose vertices are the
     * ones that miss the pixel, and one that reads the count wrong draws
     * nothing. The second command is the one that covers it. */
    /* Resolved rather than linked: Mesa's non-glvnd libGL does not export
     * these, so a direct call fails at load with "undefined symbol" and takes
     * the whole probe with it. */
    void (*multi_arrays_count)(GLenum, GLintptr, GLintptr, GLsizei, GLsizei) =
        (void (*)(GLenum, GLintptr, GLintptr, GLsizei, GLsizei))
            eglGetProcAddress("glMultiDrawArraysIndirectCount");
    void (*multi_elements_count)(GLenum, GLenum, GLintptr, GLintptr, GLsizei,
                                 GLsizei) =
        (void (*)(GLenum, GLenum, GLintptr, GLintptr, GLsizei, GLsizei))
            eglGetProcAddress("glMultiDrawElementsIndirectCount");
    if (have_extension("GL_ARB_indirect_parameters") && multi_arrays_count &&
        multi_elements_count) {
        const struct draw_arrays_command counted[] = {
            {0, 1, 0, 0}, {3, 1, 0, 7}, {3, 1, 0, 11},
        };
        const GLuint how_many = 2;
        GLuint count_buffer = 0;
        glGenBuffers(1, &count_buffer);
        glBindBuffer(GL_PARAMETER_BUFFER, count_buffer);
        glBufferData(GL_PARAMETER_BUFFER, sizeof(how_many), &how_many,
                     GL_STATIC_DRAW);
        glBindBuffer(GL_DRAW_INDIRECT_BUFFER, indirect_buffer);
        glBufferData(GL_DRAW_INDIRECT_BUFFER, sizeof(counted), counted,
                     GL_STATIC_DRAW);
        glClear(GL_COLOR_BUFFER_BIT);
        multi_arrays_count(GL_TRIANGLES, 0, 0, 3, sizeof(counted[0]));
        check_green("glMultiDrawArraysIndirectCount");

        const struct draw_elements_command counted_elements[] = {
            {0, 1, 0, 0, 0}, {3, 1, 0, 0, 9}, {3, 1, 0, 0, 13},
        };
        glBufferData(GL_DRAW_INDIRECT_BUFFER, sizeof(counted_elements),
                     counted_elements, GL_STATIC_DRAW);
        glClear(GL_COLOR_BUFFER_BIT);
        multi_elements_count(GL_TRIANGLES, GL_UNSIGNED_SHORT, 0, 0, 3,
                             sizeof(counted_elements[0]));
        check_green("glMultiDrawElementsIndirectCount");
        glDeleteBuffers(1, &count_buffer);
    } else {
        fprintf(stderr, "INFO GL_ARB_indirect_parameters absent; the count "
                        "variants are not tested\n");
    }

    puts("gl44 indirect draw backend ok");
    return 0;
}
