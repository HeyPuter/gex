#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(condition, message) do { \
    if (!(condition)) { \
        fprintf(stderr, "texture/sampler probe failed: %s (line %d, error=0x%x)\n", \
                message, __LINE__, glGetError()); \
        return __LINE__; \
    } \
} while (0)

static GLuint compile_shader(GLenum type, const char *source) {
    GLuint shader = glCreateShader(type);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[2048] = {0};
        glGetShaderInfoLog(shader, sizeof(log), NULL, log);
        fprintf(stderr, "texture/sampler shader failed: %s\n", log);
        exit(2);
    }
    return shader;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    EGLConfig config = NULL; EGLint config_count = 0;
    CHECK(eglChooseConfig(display, NULL, &config, 1, &config_count), "eglChooseConfig");
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    const EGLint pbuffer_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(display, config, pbuffer_attributes);
    CHECK(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    GLint values[8] = {-1, -1, -1, -1, -1, -1, -1, -1};
    glGetInternalformativ(GL_TEXTURE_2D, GL_RGBA8, GL_INTERNALFORMAT_SUPPORTED, 1, values);
    CHECK(values[0] == GL_TRUE, "RGBA8 internal format support");
    glGetInternalformativ(GL_TEXTURE_2D, GL_RGBA8, GL_INTERNALFORMAT_RED_SIZE, 1, values);
    CHECK(values[0] == 8, "RGBA8 red size");
    glGetInternalformativ(GL_RENDERBUFFER, GL_RGBA8, GL_NUM_SAMPLE_COUNTS, 1, values);
    CHECK(values[0] >= 1 && values[0] <= 8, "bounded renderbuffer sample-count list");
    const GLint sample_count = values[0];
    memset(values, 0, sizeof(values));
    glGetInternalformativ(GL_RENDERBUFFER, GL_RGBA8, GL_SAMPLES, 8, values);
    CHECK(values[0] >= 1, "renderbuffer sample counts");
    GLuint multisample_fbo = 0, multisample_color = 0;
    glGenFramebuffers(1, &multisample_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, multisample_fbo);
    glGenRenderbuffers(1, &multisample_color);
    glBindRenderbuffer(GL_RENDERBUFFER, multisample_color);
    glRenderbufferStorageMultisample(GL_RENDERBUFFER, sample_count, GL_RGBA8, 1, 1);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER,
                              multisample_color);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "multisample framebuffer completeness");
    GLfloat sample_position[2] = {-1, -1};
    glGetMultisamplefv(GL_SAMPLE_POSITION, 0, sample_position);
    CHECK(sample_position[0] >= 0 && sample_position[0] <= 1 &&
          sample_position[1] >= 0 && sample_position[1] <= 1,
          "multisample position range");

    GLuint samplers[3] = {0, 0, 0};
    glGenSamplers(3, samplers);
    CHECK(samplers[0] && samplers[1] && samplers[2], "sampler allocation");
    glBindSamplers(2, 3, samplers);
    for (GLuint unit = 0; unit < 3; ++unit) {
        GLint binding = -1;
        glGetIntegeri_v(GL_SAMPLER_BINDING, unit + 2, &binding);
        if ((GLuint)binding != samplers[unit])
            fprintf(stderr, "sampler unit %u expected guest %u, got %d\n",
                    unit + 2, samplers[unit], binding);
        CHECK((GLuint)binding == samplers[unit], "multi-bind returns guest sampler name");
    }
    glBindSamplers(2, 3, NULL);
    for (GLuint unit = 0; unit < 3; ++unit) {
        GLint binding = -1;
        glGetIntegeri_v(GL_SAMPLER_BINDING, unit + 2, &binding);
        CHECK(binding == 0, "null multi-bind unbinds sampler range");
    }

    const GLuint unsigned_border[4] = {0, 255, 0, 255};
    glSamplerParameterIuiv(samplers[0], GL_TEXTURE_BORDER_COLOR, unsigned_border);
    GLuint returned_unsigned_border[4] = {1, 1, 1, 1};
    glGetSamplerParameterIuiv(samplers[0], GL_TEXTURE_BORDER_COLOR, returned_unsigned_border);
    CHECK(memcmp(unsigned_border, returned_unsigned_border, sizeof(unsigned_border)) == 0,
          "unsigned sampler border-color round trip");
    const GLint signed_border[4] = {-7, 8, -9, 10};
    glSamplerParameterIiv(samplers[1], GL_TEXTURE_BORDER_COLOR, signed_border);
    GLint returned_signed_border[4] = {0, 0, 0, 0};
    glGetSamplerParameterIiv(samplers[1], GL_TEXTURE_BORDER_COLOR, returned_signed_border);
    CHECK(memcmp(signed_border, returned_signed_border, sizeof(signed_border)) == 0,
          "signed sampler border-color round trip");

    GLuint texture = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    const unsigned char black[4] = {0, 0, 0, 255};
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, black);
    glTexParameterIuiv(GL_TEXTURE_2D, GL_TEXTURE_BORDER_COLOR, unsigned_border);
    memset(returned_unsigned_border, 0, sizeof(returned_unsigned_border));
    glGetTexParameterIuiv(GL_TEXTURE_2D, GL_TEXTURE_BORDER_COLOR, returned_unsigned_border);
    CHECK(memcmp(unsigned_border, returned_unsigned_border, sizeof(unsigned_border)) == 0,
          "unsigned texture border-color round trip");

    glSamplerParameteri(samplers[0], GL_TEXTURE_WRAP_S, GL_CLAMP_TO_BORDER);
    glSamplerParameteri(samplers[0], GL_TEXTURE_WRAP_T, GL_CLAMP_TO_BORDER);
    glSamplerParameteri(samplers[0], GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glSamplerParameteri(samplers[0], GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    const GLfloat float_border[4] = {0, 1, 0, 1};
    glSamplerParameterfv(samplers[0], GL_TEXTURE_BORDER_COLOR, float_border);
    glBindSamplers(0, 1, samplers);

    const char *vertex_source =
        "#version 440 core\n"
        "const vec2 p[3] = vec2[3](vec2(-1,-1),vec2(3,-1),vec2(-1,3));\n"
        "void main(){gl_Position=vec4(p[gl_VertexID],0,1);}\n";
    const char *fragment_source =
        "#version 440 core\n"
        "layout(binding=0) uniform sampler2D source_texture;\n"
        "uniform vec2 sample_coord;\n"
        "out vec4 color;\n"
        "void main(){color=texture(source_texture,sample_coord);}\n";
    GLuint program = glCreateProgram();
    GLuint vertex_shader = compile_shader(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment_shader = compile_shader(GL_FRAGMENT_SHADER, fragment_source);
    glAttachShader(program, vertex_shader);
    glAttachShader(program, fragment_shader);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    CHECK(linked, "integer sampler program link");
    glUseProgram(program);
    GLint sample_coord = glGetUniformLocation(program, "sample_coord");
    CHECK(sample_coord >= 0, "sample coordinate uniform");
    glUniform2f(sample_coord, 2.0f, 2.0f);

    GLuint color_texture = 0, framebuffer = 0, vao = 0;
    glGenTextures(1, &color_texture);
    glBindTexture(GL_TEXTURE_2D, color_texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           color_texture, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "framebuffer completeness");
    glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D, texture);
    glViewport(0, 0, 1, 1);
    glClearColor(1, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    unsigned char pixel[4] = {0, 0, 0, 0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    if (!(pixel[0] <= 2 && pixel[1] >= 253 && pixel[2] <= 2 && pixel[3] >= 253))
        fprintf(stderr, "border sample returned rgba=(%u,%u,%u,%u)\n",
                pixel[0], pixel[1], pixel[2], pixel[3]);
    CHECK(pixel[0] <= 2 && pixel[1] >= 253 && pixel[2] <= 2 && pixel[3] >= 253,
          "clamp-to-border sampled color");

    /* Sampler state is mutable after link and after the program's first draw. */
    const GLfloat blue_border[4] = {0, 0, 1, 1};
    glSamplerParameterfv(samplers[0], GL_TEXTURE_BORDER_COLOR, blue_border);
    glClearColor(1, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    memset(pixel, 0, sizeof(pixel));
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    CHECK(pixel[0] <= 2 && pixel[1] <= 2 && pixel[2] >= 253 && pixel[3] >= 253,
          "post-link sampler border mutation");

    glSamplerParameteri(samplers[0], GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glSamplerParameteri(samplers[0], GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glClearColor(1, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    memset(pixel, 0, sizeof(pixel));
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    CHECK(pixel[0] <= 2 && pixel[1] <= 2 && pixel[2] <= 2 && pixel[3] >= 253,
          "post-link sampler wrap mutation");

    glSamplerParameteri(samplers[0], GL_TEXTURE_WRAP_S, GL_CLAMP_TO_BORDER);
    glSamplerParameteri(samplers[0], GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glSamplerParameteri(samplers[0], GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glSamplerParameteri(samplers[0], GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glSamplerParameterfv(samplers[0], GL_TEXTURE_BORDER_COLOR, float_border);
    glUniform2f(sample_coord, -0.25f, 0.5f);
    glClearColor(1, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    memset(pixel, 0, sizeof(pixel));
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    CHECK(pixel[0] <= 2 && pixel[1] >= 189 && pixel[1] <= 193 &&
          pixel[2] <= 2 && pixel[3] >= 253,
          "linear clamp-to-border edge blend");
    CHECK(glGetError() == GL_NO_ERROR, "unexpected final GL error");
    printf("gl44 texture/sampler transport ok (sample-counts=%d)\n", sample_count);
    return 0;
}
