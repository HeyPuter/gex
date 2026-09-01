#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "state query failed: %s\n", message); return __LINE__; } \
} while (0)

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    EGLConfig config = NULL;
    EGLint config_count = 0;
    CHECK(eglChooseConfig(display, NULL, &config, 1, &config_count), "eglChooseConfig");
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, NULL);
    CHECK(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    CHECK(!glIsEnabled(GL_BLEND), "blend initially disabled");
    glEnable(GL_BLEND);
    CHECK(glIsEnabled(GL_BLEND), "blend enabled");
    glDisable(GL_BLEND);
    CHECK(!glIsEnabled(GL_BLEND), "blend disabled again");
    glEnablei(GL_BLEND, 0);
    CHECK(glIsEnabledi(GL_BLEND, 0), "indexed blend enabled");
    glDisablei(GL_BLEND, 0);
    CHECK(!glIsEnabledi(GL_BLEND, 0), "indexed blend disabled");

    GLuint buffer = 0;
    glGenBuffers(1, &buffer);
    CHECK(!glIsBuffer(buffer), "generated buffer is not an object before bind");
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    CHECK(glIsBuffer(buffer), "bound buffer is an object");
    glDeleteBuffers(1, &buffer);
    CHECK(!glIsBuffer(buffer), "deleted buffer is not an object");

    GLuint texture = 0;
    glGenTextures(1, &texture);
    CHECK(!glIsTexture(texture), "generated texture is not an object before bind");
    glBindTexture(GL_TEXTURE_2D, texture);
    CHECK(glIsTexture(texture), "bound texture is an object");
    glDeleteTextures(1, &texture);
    CHECK(!glIsTexture(texture), "deleted texture is not an object");

    GLuint framebuffer = 0;
    glGenFramebuffers(1, &framebuffer);
    CHECK(!glIsFramebuffer(framebuffer), "generated framebuffer is not an object before bind");
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    CHECK(glIsFramebuffer(framebuffer), "bound framebuffer is an object");
    glDeleteFramebuffers(1, &framebuffer);
    CHECK(!glIsFramebuffer(framebuffer), "deleted framebuffer is not an object");

    GLuint renderbuffer = 0;
    glGenRenderbuffers(1, &renderbuffer);
    CHECK(!glIsRenderbuffer(renderbuffer), "generated renderbuffer is not an object before bind");
    glBindRenderbuffer(GL_RENDERBUFFER, renderbuffer);
    CHECK(glIsRenderbuffer(renderbuffer), "bound renderbuffer is an object");
    glDeleteRenderbuffers(1, &renderbuffer);
    CHECK(!glIsRenderbuffer(renderbuffer), "deleted renderbuffer is not an object");

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    CHECK(!glIsVertexArray(vao), "generated VAO is not an object before bind");
    glBindVertexArray(vao);
    CHECK(glIsVertexArray(vao), "bound VAO is an object");
    glDeleteVertexArrays(1, &vao);
    CHECK(!glIsVertexArray(vao), "deleted VAO is not an object");

    GLuint sampler = 0;
    glGenSamplers(1, &sampler);
    CHECK(glIsSampler(sampler), "generated sampler is an object");
    glDeleteSamplers(1, &sampler);
    CHECK(!glIsSampler(sampler), "deleted sampler is not an object");

    GLuint shader = glCreateShader(GL_VERTEX_SHADER);
    GLuint program = glCreateProgram();
    CHECK(glIsShader(shader), "created shader is an object");
    CHECK(glIsProgram(program), "created program is an object");
    glDeleteShader(shader);
    glDeleteProgram(program);
    CHECK(!glIsShader(shader), "deleted shader is not an object");
    CHECK(!glIsProgram(program), "deleted program is not an object");
    CHECK(!glIsBuffer(0) && !glIsTexture(0) && !glIsProgram(0), "zero is never an object");

    GLuint query_buffer = 0;
    glGenBuffers(1, &query_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, query_buffer);
    glBufferData(GL_ARRAY_BUFFER, 37, NULL, GL_DYNAMIC_DRAW);
    struct { GLint before, value, after; } scalar = {0x1357, -1, 0x2468};
    glGetBufferParameteriv(GL_ARRAY_BUFFER, GL_BUFFER_SIZE, &scalar.value);
    CHECK(scalar.value == 37 && scalar.before == 0x1357 && scalar.after == 0x2468,
          "buffer scalar query arity/value");
    GLint64 buffer_size64 = -1;
    glGetBufferParameteri64v(GL_ARRAY_BUFFER, GL_BUFFER_SIZE, &buffer_size64);
    CHECK(buffer_size64 == 37, "64-bit buffer size query");

    GLuint query_texture = 0;
    glGenTextures(1, &query_texture);
    glBindTexture(GL_TEXTURE_2D, query_texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameterf(GL_TEXTURE_2D, GL_TEXTURE_LOD_BIAS, 1.25f);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 7, 3, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    GLint texture_int = 0;
    GLfloat texture_float = 0.0f;
    glGetTexParameteriv(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, &texture_int);
    CHECK(texture_int == GL_NEAREST, "integer texture parameter query");
    glGetTexParameterfv(GL_TEXTURE_2D, GL_TEXTURE_LOD_BIAS, &texture_float);
    CHECK(texture_float == 1.25f, "float texture parameter query");
    GLint texture_border_i[6] = {0x1111, -1, -1, -1, -1, 0x2222};
    GLuint texture_border_ui[6] = {0x3333, 1, 1, 1, 1, 0x4444};
    glGetTexParameterIiv(GL_TEXTURE_2D, GL_TEXTURE_BORDER_COLOR, texture_border_i + 1);
    glGetTexParameterIuiv(GL_TEXTURE_2D, GL_TEXTURE_BORDER_COLOR, texture_border_ui + 1);
    CHECK(texture_border_i[0] == 0x1111 && texture_border_i[5] == 0x2222 &&
          texture_border_ui[0] == 0x3333 && texture_border_ui[5] == 0x4444,
          "texture vector query arity");
    for (int i = 1; i <= 4; ++i)
        CHECK(texture_border_i[i] == 0 && texture_border_ui[i] == 0,
              "default integer texture border color");
    GLint texture_width = 0;
    GLfloat texture_height = 0.0f;
    glGetTexLevelParameteriv(GL_TEXTURE_2D, 0, GL_TEXTURE_WIDTH, &texture_width);
    glGetTexLevelParameterfv(GL_TEXTURE_2D, 0, GL_TEXTURE_HEIGHT, &texture_height);
    CHECK(texture_width == 7 && texture_height == 3.0f, "texture level dimensions");

    GLuint query_sampler = 0;
    glGenSamplers(1, &query_sampler);
    glSamplerParameteri(query_sampler, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glSamplerParameterf(query_sampler, GL_TEXTURE_LOD_BIAS, 2.5f);
    GLint sampler_int = 0, sampler_border_i[4] = {-1, -1, -1, -1};
    GLuint sampler_border_ui[4] = {1, 1, 1, 1};
    GLfloat sampler_float = 0.0f;
    glGetSamplerParameteriv(query_sampler, GL_TEXTURE_WRAP_S, &sampler_int);
    glGetSamplerParameterfv(query_sampler, GL_TEXTURE_LOD_BIAS, &sampler_float);
    glGetSamplerParameterIiv(query_sampler, GL_TEXTURE_BORDER_COLOR, sampler_border_i);
    glGetSamplerParameterIuiv(query_sampler, GL_TEXTURE_BORDER_COLOR, sampler_border_ui);
    CHECK(sampler_int == GL_CLAMP_TO_EDGE && sampler_float == 2.5f,
          "sampler scalar parameter queries");
    for (int i = 0; i < 4; ++i)
        CHECK(sampler_border_i[i] == 0 && sampler_border_ui[i] == 0,
              "sampler vector parameter queries");

    GLuint query_framebuffer = 0;
    glGenFramebuffers(1, &query_framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, query_framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, query_texture, 0);
    GLint attachment_name = 0;
    glGetFramebufferAttachmentParameteriv(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                          GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
                                          &attachment_name);
    CHECK((GLuint)attachment_name == query_texture, "framebuffer attachment guest object name");

    GLuint query_renderbuffer = 0;
    glGenRenderbuffers(1, &query_renderbuffer);
    glBindRenderbuffer(GL_RENDERBUFFER, query_renderbuffer);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 11, 5);
    GLint renderbuffer_width = 0;
    glGetRenderbufferParameteriv(GL_RENDERBUFFER, GL_RENDERBUFFER_WIDTH, &renderbuffer_width);
    CHECK(renderbuffer_width == 11, "renderbuffer width query");

    GLuint query_vao = 0;
    glGenVertexArrays(1, &query_vao);
    glBindVertexArray(query_vao);
    glBindBuffer(GL_ARRAY_BUFFER, query_buffer);
    glVertexAttribPointer(3, 2, GL_FLOAT, GL_FALSE, 12, (const void *)4);
    glVertexAttrib4f(3, 0.25f, 0.5f, 0.75f, 1.0f);
    GLfloat vertex_f[4] = {0};
    GLint vertex_buffer_name = 0;
    glGetVertexAttribfv(3, GL_CURRENT_VERTEX_ATTRIB, vertex_f);
    glGetVertexAttribiv(3, GL_VERTEX_ATTRIB_ARRAY_BUFFER_BINDING, &vertex_buffer_name);
    CHECK(vertex_f[0] == 0.25f && vertex_f[1] == 0.5f &&
          vertex_f[2] == 0.75f && vertex_f[3] == 1.0f,
          "float current vertex attribute query");
    CHECK((GLuint)vertex_buffer_name == query_buffer, "vertex buffer guest object name");
    glVertexAttribI4ui(4, 11, 22, 33, 44);
    GLint vertex_i[4] = {0};
    GLuint vertex_ui[4] = {0};
    glGetVertexAttribIiv(4, GL_CURRENT_VERTEX_ATTRIB, vertex_i);
    glGetVertexAttribIuiv(4, GL_CURRENT_VERTEX_ATTRIB, vertex_ui);
    for (int i = 0; i < 4; ++i) {
        CHECK(vertex_i[i] == 11 + i * 11, "signed integer current vertex attribute query");
        CHECK(vertex_ui[i] == (GLuint)(11 + i * 11),
              "unsigned integer current vertex attribute query");
    }

    CHECK(glGetError() == GL_NO_ERROR, "unexpected GL error");
    puts("gl44 state query transport ok");
    return 0;
}
