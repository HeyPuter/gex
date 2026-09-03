#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

/* Mesa's non-glvnd libGL exports a frozen legacy symbol list -- 1369 names in
 * src/glx/libgl-symbols.txt -- and glGetProgramResourceLocationIndex is not on it, nor is it
 * anywhere in the library. Linking it resolves against the system's glvnd
 * libGL at build time and then dies at load against the driver's:
 *
 *   symbol lookup error: undefined symbol: glGetProgramResourceLocationIndex
 *
 * which took this probe's whole result with it, silently. Every real toolkit
 * resolves modern entry points through glXGetProcAddress or eglGetProcAddress
 * -- Minecraft's GLFW among them -- so do the same rather than depend on a
 * list that has not moved since GL 4.3. */
static GLint (*resolved_getprogramresourcelocationindex)(GLuint, GLenum, const char *);
#define glGetProgramResourceLocationIndex resolved_getprogramresourcelocationindex
static void (*resolved_shaderstorageblockbinding)(GLuint, GLuint, GLuint);
#define glShaderStorageBlockBinding resolved_shaderstorageblockbinding

#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "reflection query failed: %s (line %d)\n", message, __LINE__); return __LINE__; } \
} while (0)

static GLuint shader(GLenum stage, const char *source) {
    GLuint object = glCreateShader(stage);
    glShaderSource(object, 1, &source, NULL);
    glCompileShader(object);
    GLint ok = 0;
    glGetShaderiv(object, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[2048]; GLsizei length = 0;
        glGetShaderInfoLog(object, sizeof(log), &length, log);
        fprintf(stderr, "shader compile failed: %.*s\n", length, log);
        return 0;
    }
    return object;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    EGLConfig config = NULL; EGLint config_count = 0;
    CHECK(eglChooseConfig(display, NULL, &config, 1, &config_count), "eglChooseConfig");
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, NULL);
    CHECK(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");
    resolved_getprogramresourcelocationindex = (GLint (*)(GLuint, GLenum, const char *))
        eglGetProcAddress("glGetProgramResourceLocationIndex");
    CHECK(resolved_getprogramresourcelocationindex != NULL, "glGetProgramResourceLocationIndex unavailable");
    resolved_shaderstorageblockbinding = (void (*)(GLuint, GLuint, GLuint))
        eglGetProcAddress("glShaderStorageBlockBinding");
    CHECK(resolved_shaderstorageblockbinding != NULL, "glShaderStorageBlockBinding unavailable");

    const char *vertex_source =
        "#version 430 core\n"
        "layout(location=2) in vec2 position;\n"
        "layout(std140, binding=3) uniform Camera { mat4 projection; };\n"
        "layout(std430, binding=4) buffer Values { float value; };\n"
        "void main() { gl_Position = projection * vec4(position + vec2(value), 0.0, 1.0); }\n";
    const char *fragment_source =
        "#version 430 core\n"
        "uniform vec4 tint;\n"
        "layout(location=1) out vec4 color;\n"
        "void main() { color = tint; }\n";
    GLuint vertex = shader(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = shader(GL_FRAGMENT_SHADER, fragment_source);
    CHECK(vertex && fragment, "shader compilation");
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex); glAttachShader(program, fragment); glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[2048]; GLsizei length = 0;
        glGetProgramInfoLog(program, sizeof(log), &length, log);
        fprintf(stderr, "program link failed: %.*s\n", length, log);
    }
    CHECK(linked, "program link");

    GLint count = 0, maximum = 0;
    glGetProgramInterfaceiv(program, GL_PROGRAM_INPUT, GL_ACTIVE_RESOURCES, &count);
    printf("program inputs: %d\n", count);
    CHECK(count == 1, "one active program input");
    glGetProgramInterfaceiv(program, GL_PROGRAM_OUTPUT, GL_ACTIVE_RESOURCES, &count);
    printf("program outputs: %d\n", count);
    CHECK(count == 1, "one active program output");
    glGetProgramInterfaceiv(program, GL_UNIFORM_BLOCK, GL_ACTIVE_RESOURCES, &count);
    printf("uniform blocks: %d\n", count);
    CHECK(count == 1, "one active uniform block");
    glGetProgramInterfaceiv(program, GL_SHADER_STORAGE_BLOCK, GL_ACTIVE_RESOURCES, &count);
    printf("storage blocks: %d\n", count);
    CHECK(count == 1, "one active shader storage block");
    glGetProgramInterfaceiv(program, GL_BUFFER_VARIABLE, GL_ACTIVE_RESOURCES, &count);
    printf("buffer variables: %d\n", count);
    CHECK(count == 1, "one active buffer variable");
    glGetProgramInterfaceiv(program, GL_SHADER_STORAGE_BLOCK, GL_MAX_NAME_LENGTH, &maximum);
    CHECK(maximum >= 7, "storage block maximum name length");

    GLuint camera = glGetProgramResourceIndex(program, GL_UNIFORM_BLOCK, "Camera");
    GLuint values = glGetProgramResourceIndex(program, GL_SHADER_STORAGE_BLOCK, "Values");
    GLuint value = glGetProgramResourceIndex(program, GL_BUFFER_VARIABLE, "value");
    CHECK(camera != GL_INVALID_INDEX && values != GL_INVALID_INDEX && value != GL_INVALID_INDEX,
          "named resource indices");
    CHECK(glGetProgramResourceLocation(program, GL_PROGRAM_INPUT, "position") == 2,
          "explicit input location");
    CHECK(glGetProgramResourceLocation(program, GL_PROGRAM_OUTPUT, "color") == 1,
          "explicit output location");
    CHECK(glGetProgramResourceLocationIndex(program, GL_PROGRAM_OUTPUT, "color") == 0,
          "output location index");
    CHECK(glGetProgramResourceLocation(program, GL_UNIFORM, "tint") >= 0,
          "plain uniform location");

    char name[32]; GLsizei length = 0;
    glGetProgramResourceName(program, GL_SHADER_STORAGE_BLOCK, values, sizeof(name), &length, name);
    CHECK(length == 6 && strcmp(name, "Values") == 0, "resource name round trip");
    char truncated[4];
    glGetProgramResourceName(program, GL_SHADER_STORAGE_BLOCK, values, sizeof(truncated), &length, truncated);
    CHECK(length == 3 && strcmp(truncated, "Val") == 0, "resource name truncation");

    const GLenum block_props[] = {GL_BUFFER_BINDING, GL_BUFFER_DATA_SIZE, GL_NUM_ACTIVE_VARIABLES,
                                  GL_REFERENCED_BY_VERTEX_SHADER};
    GLint block_values[4] = {0};
    glGetProgramResourceiv(program, GL_SHADER_STORAGE_BLOCK, values, 4, block_props, 4, &length, block_values);
    CHECK(length == 4 && block_values[0] == 4 && block_values[1] >= 4 &&
          block_values[2] == 1 && block_values[3] == GL_TRUE, "storage block properties");
    glShaderStorageBlockBinding(program, values, 5);
    const GLenum binding_prop = GL_BUFFER_BINDING; GLint binding = 0;
    glGetProgramResourceiv(program, GL_SHADER_STORAGE_BLOCK, values, 1, &binding_prop, 1, &length, &binding);
    CHECK(binding == 5, "storage block binding update");

    const GLenum variable_props[] = {GL_TYPE, GL_OFFSET, GL_BLOCK_INDEX};
    GLint variable_values[3] = {0};
    glGetProgramResourceiv(program, GL_BUFFER_VARIABLE, value, 3, variable_props, 3, &length, variable_values);
    CHECK(length == 3 && variable_values[0] == GL_FLOAT && variable_values[1] == 0 &&
          variable_values[2] == (GLint)values, "buffer variable properties");
    CHECK(glGetError() == GL_NO_ERROR, "unexpected GL error");
    puts("gl44 program reflection transport ok");
    return 0;
}
