#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    const char *name;
    unsigned char expected[4];
    unsigned char tolerance;
} Expected;

static int failures;
static int cases;

static void check_gl(const char *where) {
    GLenum error = glGetError();
    if (error != GL_NO_ERROR) {
        fprintf(stderr, "FATAL %s gl_error=0x%x\n", where, error);
        exit(2);
    }
}

static GLuint compile_shader(GLenum type, const char *source) {
    GLuint shader = glCreateShader(type);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = GL_FALSE;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[4096] = {0};
        glGetShaderInfoLog(shader, sizeof(log), NULL, log);
        fprintf(stderr, "FATAL shader_compile type=0x%x log=%s\n", type, log);
        exit(2);
    }
    return shader;
}

static GLuint make_program(GLuint vertex, const char *expression) {
    char source[2048];
    snprintf(source, sizeof(source),
             "#version 440 core\n"
             "layout(binding=0) uniform sampler2D source_texture;\n"
             "uniform vec2 sample_coord;\n"
             "uniform float sample_lod;\n"
             "layout(location=0) out vec4 color;\n"
             "void main(){color=%s;}\n", expression);
    GLuint fragment = compile_shader(GL_FRAGMENT_SHADER, source);
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint ok = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[4096] = {0};
        glGetProgramInfoLog(program, sizeof(log), NULL, log);
        fprintf(stderr, "FATAL program_link expression=%s log=%s\n", expression, log);
        exit(2);
    }
    glDeleteShader(fragment);
    return program;
}

static void report(Expected expected, const unsigned char actual[4]) {
    int pass = 1;
    for (int channel = 0; channel < 4; ++channel) {
        int delta = (int)actual[channel] - (int)expected.expected[channel];
        if (delta < 0) delta = -delta;
        if (delta > expected.tolerance) pass = 0;
    }
    ++cases;
    if (!pass) ++failures;
    printf("CASE %-34s %s expected=%u,%u,%u,%u actual=%u,%u,%u,%u tolerance=%u\n",
           expected.name, pass ? "PASS" : "FAIL",
           expected.expected[0], expected.expected[1], expected.expected[2],
           expected.expected[3], actual[0], actual[1], actual[2], actual[3],
           expected.tolerance);
}

static void sample(GLuint program, GLuint texture, GLuint sampler, float s, float t,
                   float lod, Expected expected) {
    glUseProgram(program);
    GLint coord_location = glGetUniformLocation(program, "sample_coord");
    GLint lod_location = glGetUniformLocation(program, "sample_lod");
    if (coord_location >= 0) glUniform2f(coord_location, s, t);
    if (lod_location >= 0) glUniform1f(lod_location, lod);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, texture);
    glBindSampler(0, sampler);
    glClearColor(0.125f, 0.25f, 0.5f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    unsigned char pixel[4] = {0, 0, 0, 0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    check_gl(expected.name);
    report(expected, pixel);
}

static void base_texture_state(GLuint texture) {
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_BASE_LEVEL, 0);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 1);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    const GLint identity[4] = {GL_RED, GL_GREEN, GL_BLUE, GL_ALPHA};
    glTexParameteriv(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_RGBA, identity);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "FATAL egl_initialize error=0x%x\n", eglGetError());
        return 2;
    }
    EGLConfig config = NULL;
    EGLint config_count = 0;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count) ||
        config_count < 1) {
        fprintf(stderr, "FATAL egl_config error=0x%x\n", eglGetError());
        return 2;
    }
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    const EGLint surface_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) {
        fprintf(stderr, "FATAL egl_context error=0x%x\n", eglGetError());
        return 2;
    }

    printf("INFO vendor=%s renderer=%s version=%s\n", glGetString(GL_VENDOR),
           glGetString(GL_RENDERER), glGetString(GL_VERSION));

    const char *vertex_source =
        "#version 440 core\n"
        "const vec2 p[3]=vec2[3](vec2(-1,-1),vec2(3,-1),vec2(-1,3));\n"
        "void main(){gl_Position=vec4(p[gl_VertexID],0,1);}\n";
    GLuint vertex = compile_shader(GL_VERTEX_SHADER, vertex_source);
    GLuint implicit_program = make_program(vertex, "texture(source_texture,sample_coord)");
    GLuint lod_program = make_program(vertex, "textureLod(source_texture,sample_coord,sample_lod)");
    glDeleteShader(vertex);

    GLuint output = 0, framebuffer = 0, vao = 0;
    glGenTextures(1, &output);
    glBindTexture(GL_TEXTURE_2D, output);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           output, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "FATAL framebuffer_incomplete\n");
        return 2;
    }
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glViewport(0, 0, 1, 1);

    /* Rows are specified in OpenGL's lower-left texture order. */
    const unsigned char level0[16] = {
        255, 0, 0, 255,   0, 255, 0, 255,
        0, 0, 255, 255,   255, 255, 255, 255,
    };
    const unsigned char level1[4] = {255, 255, 0, 255};
    GLuint texture = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 2, 2, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, level0);
    glTexImage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, level1);

    base_texture_state(texture);
    sample(implicit_program, texture, 0, 0.25f, 0.25f, 0,
           (Expected){"nearest_center", {255, 0, 0, 255}, 0});
    sample(implicit_program, texture, 0, -0.25f, 0.25f, 0,
           (Expected){"nearest_clamp_edge", {255, 0, 0, 255}, 0});

    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_REPEAT);
    sample(implicit_program, texture, 0, 1.75f, 0.25f, 0,
           (Expected){"nearest_repeat", {0, 255, 0, 255}, 0});

    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_MIRRORED_REPEAT);
    sample(implicit_program, texture, 0, 1.25f, 0.25f, 0,
           (Expected){"nearest_mirrored_repeat", {0, 255, 0, 255}, 0});

    const GLfloat magenta[4] = {1, 0, 1, 1};
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_BORDER);
    glTexParameterfv(GL_TEXTURE_2D, GL_TEXTURE_BORDER_COLOR, magenta);
    GLfloat queried_border[4] = {-1, -1, -1, -1};
    GLint queried_sampler = -1;
    glGetTexParameterfv(GL_TEXTURE_2D, GL_TEXTURE_BORDER_COLOR, queried_border);
    glGetIntegeri_v(GL_SAMPLER_BINDING, 0, &queried_sampler);
    const int border_state_pass = queried_border[0] == 1.0f &&
                                  queried_border[1] == 0.0f &&
                                  queried_border[2] == 1.0f &&
                                  queried_border[3] == 1.0f && queried_sampler == 0;
    ++cases;
    if (!border_state_pass) ++failures;
    printf("CASE %-34s %s border=%g,%g,%g,%g sampler_binding=%d\n",
           "texture_border_query", border_state_pass ? "PASS" : "FAIL",
           queried_border[0], queried_border[1], queried_border[2], queried_border[3],
           queried_sampler);
    sample(implicit_program, texture, 0, -0.25f, 0.25f, 0,
           (Expected){"nearest_clamp_border", {255, 0, 255, 255}, 0});

    base_texture_state(texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    sample(implicit_program, texture, 0, 0.5f, 0.25f, 0,
           (Expected){"linear_between_texels", {128, 128, 0, 255}, 1});

    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_BORDER);
    glTexParameterfv(GL_TEXTURE_2D, GL_TEXTURE_BORDER_COLOR, magenta);
    sample(implicit_program, texture, 0, 0.0f, 0.25f, 0,
           (Expected){"linear_border_blend", {255, 0, 128, 255}, 1});

    base_texture_state(texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST_MIPMAP_NEAREST);
    sample(lod_program, texture, 0, 0.25f, 0.25f, 1.0f,
           (Expected){"explicit_lod_level_1", {255, 255, 0, 255}, 0});
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR_MIPMAP_LINEAR);
    sample(lod_program, texture, 0, 0.25f, 0.25f, 0.5f,
           (Expected){"trilinear_half_lod", {255, 128, 0, 255}, 1});

    base_texture_state(texture);
    const GLint bgra[4] = {GL_BLUE, GL_GREEN, GL_RED, GL_ALPHA};
    glTexParameteriv(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_RGBA, bgra);
    sample(implicit_program, texture, 0, 0.25f, 0.25f, 0,
           (Expected){"post_upload_swizzle", {0, 0, 255, 255}, 0});

    base_texture_state(texture);
    GLuint sampler = 0;
    glGenSamplers(1, &sampler);
    glSamplerParameteri(sampler, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glSamplerParameteri(sampler, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glSamplerParameteri(sampler, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_BORDER);
    glSamplerParameteri(sampler, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glSamplerParameterfv(sampler, GL_TEXTURE_BORDER_COLOR, magenta);
    sample(implicit_program, texture, sampler, -0.25f, 0.25f, 0,
           (Expected){"sampler_object_override", {255, 0, 255, 255}, 0});
    const GLfloat cyan[4] = {0, 1, 1, 1};
    glSamplerParameterfv(sampler, GL_TEXTURE_BORDER_COLOR, cyan);
    sample(implicit_program, texture, sampler, -0.25f, 0.25f, 0,
           (Expected){"sampler_post_draw_mutation", {0, 255, 255, 255}, 0});
    glBindSampler(0, 0);

    base_texture_state(texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_BASE_LEVEL, 1);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 1);
    sample(lod_program, texture, 0, 0.25f, 0.25f, 0.0f,
           (Expected){"nonzero_base_level", {255, 255, 0, 255}, 0});

    GLuint srgb = 0;
    const unsigned char encoded[4] = {128, 128, 128, 255};
    glGenTextures(1, &srgb);
    glBindTexture(GL_TEXTURE_2D, srgb);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_SRGB8_ALPHA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, encoded);
    sample(implicit_program, srgb, 0, 0.5f, 0.5f, 0,
           (Expected){"srgb_decode", {55, 55, 55, 255}, 2});

    check_gl("final");
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures, failures);
    return failures == 0 ? 0 : 1;
}
