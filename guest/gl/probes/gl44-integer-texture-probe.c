#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(condition, message) do { \
    if (!(condition)) { \
        fprintf(stderr, "integer texture probe failed: %s (line %d, error=0x%x)\n", \
                message, __LINE__, glGetError()); \
        return __LINE__; \
    } \
} while (0)

static GLuint compile_shader(GLenum type, const char *source) {
    GLuint shader = glCreateShader(type);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = GL_FALSE;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[4096] = {0};
        glGetShaderInfoLog(shader, sizeof(log), NULL, log);
        fprintf(stderr, "integer texture shader failed: %s\n", log);
        exit(2);
    }
    return shader;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(display != EGL_NO_DISPLAY, "eglGetDisplay");
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    EGLConfig config = NULL;
    EGLint config_count = 0;
    CHECK(eglChooseConfig(display, NULL, &config, 1, &config_count), "eglChooseConfig");
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    CHECK(context != EGL_NO_CONTEXT, "eglCreateContext");
    const EGLint pbuffer_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(display, config, pbuffer_attributes);
    CHECK(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    const char *vertex_source =
        "#version 440 core\n"
        "const vec2 p[3] = vec2[3](vec2(-1,-1),vec2(3,-1),vec2(-1,3));\n"
        "void main(){gl_Position=vec4(p[gl_VertexID],0,1);}\n";
    const char *fragment_source =
        "#version 440 core\n"
        "layout(binding=0) uniform usampler2D source_texture;\n"
        "layout(location=0) out vec4 color;\n"
        "void main(){ivec2 s=textureSize(source_texture,0);"
        "uvec4 v=texelFetch(source_texture,s-ivec2(1,1),0);"
        "uvec4 o=texelFetchOffset(source_texture,ivec2(0),0,ivec2(1,0));"
        "color=vec4(all(equal(v,o))?v:uvec4(255,0,255,255))/255.0;}\n";

    GLuint program = glCreateProgram();
    GLuint vertex_shader = compile_shader(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment_shader = compile_shader(GL_FRAGMENT_SHADER, fragment_source);
    glAttachShader(program, vertex_shader);
    glAttachShader(program, fragment_shader);
    glLinkProgram(program);
    GLint linked = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[4096] = {0};
        glGetProgramInfoLog(program, sizeof(log), NULL, log);
        fprintf(stderr, "integer texture program failed: %s\n", log);
    }
    CHECK(linked, "program link");
    glUseProgram(program);

    GLuint source_texture = 0;
    glGenTextures(1, &source_texture);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, source_texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    const unsigned char texels[8] = {3, 5, 7, 11, 17, 93, 201, 255};
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8UI, 2, 1, 0, GL_RGBA_INTEGER,
                 GL_UNSIGNED_BYTE, texels);
    CHECK(glGetError() == GL_NO_ERROR, "RGBA8UI upload");

    GLuint output_texture = 0, framebuffer = 0, vao = 0;
    glGenTextures(1, &output_texture);
    glBindTexture(GL_TEXTURE_2D, output_texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           output_texture, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "framebuffer completeness");

    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, source_texture);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glViewport(0, 0, 1, 1);
    glClearColor(1, 0, 1, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    CHECK(glGetError() == GL_NO_ERROR, "integer texelFetch draw");

    unsigned char pixel[4] = {0, 0, 0, 0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    if (!(pixel[0] == 17 && pixel[1] == 93 && pixel[2] == 201 && pixel[3] == 255)) {
        fprintf(stderr, "integer texelFetch returned rgba=(%u,%u,%u,%u)\n",
                pixel[0], pixel[1], pixel[2], pixel[3]);
    }
    CHECK(pixel[0] == 17 && pixel[1] == 93 && pixel[2] == 201 && pixel[3] == 255,
          "integer texelFetch readback");

    /* Relinking preserves the GL object name but must invalidate translated WGSL and
     * pipelines belonging to its previous executable. */
    const char *replacement_fragment_source =
        "#version 440 core\n"
        "layout(location=0) out vec4 color;\n"
        "void main(){color=vec4(0,1,0,1);}\n";
    GLuint replacement_fragment = compile_shader(GL_FRAGMENT_SHADER, replacement_fragment_source);
    glDetachShader(program, fragment_shader);
    glAttachShader(program, replacement_fragment);
    glLinkProgram(program);
    linked = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    CHECK(linked, "same-object program relink");
    glUseProgram(program);
    glClearColor(1, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    memset(pixel, 0, sizeof(pixel));
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    CHECK(pixel[0] == 0 && pixel[1] == 255 && pixel[2] == 0 && pixel[3] == 255,
          "relink replaces cached backend shader");
    printf("gl44 integer texture texelFetch ok rgba=%u,%u,%u,%u\n",
           17u, 93u, 201u, 255u);
    return 0;
}
