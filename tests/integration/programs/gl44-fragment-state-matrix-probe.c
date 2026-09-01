#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>

static int cases;
static int failures;

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

static GLuint make_program(void) {
    const char *vertex_source =
        "#version 440 core\n"
        "uniform float ndc_depth;\n"
        "const vec2 p[3]=vec2[3](vec2(-1,-1),vec2(3,-1),vec2(-1,3));\n"
        "void main(){gl_Position=vec4(p[gl_VertexID],ndc_depth,1);}\n";
    const char *fragment_source =
        "#version 440 core\n"
        "uniform vec4 draw_color;\n"
        "layout(location=0) out vec4 color;\n"
        "void main(){color=draw_color;}\n";
    GLuint vertex = compile_shader(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile_shader(GL_FRAGMENT_SHADER, fragment_source);
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint ok = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[4096] = {0};
        glGetProgramInfoLog(program, sizeof(log), NULL, log);
        fprintf(stderr, "FATAL program_link log=%s\n", log);
        exit(2);
    }
    glDeleteShader(vertex);
    glDeleteShader(fragment);
    return program;
}

static GLuint make_mrt_program(void) {
    const char *vertex_source =
        "#version 440 core\n"
        "const vec2 p[3]=vec2[3](vec2(-1,-1),vec2(3,-1),vec2(-1,3));\n"
        "void main(){gl_Position=vec4(p[gl_VertexID],0,1);}\n";
    const char *fragment_source =
        "#version 440 core\n"
        "uniform vec4 draw_color0;\n"
        "uniform vec4 draw_color1;\n"
        "layout(location=0) out vec4 color0;\n"
        "layout(location=1) out vec4 color1;\n"
        "void main(){color0=draw_color0;color1=draw_color1;}\n";
    GLuint vertex = compile_shader(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile_shader(GL_FRAGMENT_SHADER, fragment_source);
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint ok = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[4096] = {0};
        glGetProgramInfoLog(program, sizeof(log), NULL, log);
        fprintf(stderr, "FATAL mrt_program_link log=%s\n", log);
        exit(2);
    }
    glDeleteShader(vertex);
    glDeleteShader(fragment);
    return program;
}

static int close_byte(unsigned char actual, int expected, int tolerance) {
    int delta = (int)actual - expected;
    if (delta < 0) delta = -delta;
    return delta <= tolerance;
}

static void report_color(const char *name, int r, int g, int b, int a, int tolerance) {
    unsigned char pixel[4] = {0, 0, 0, 0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    GLenum error = glGetError();
    int pass = error == GL_NO_ERROR && close_byte(pixel[0], r, tolerance) &&
               close_byte(pixel[1], g, tolerance) && close_byte(pixel[2], b, tolerance) &&
               close_byte(pixel[3], a, tolerance);
    ++cases;
    if (!pass) ++failures;
    printf("CASE %-32s %s expected=%d,%d,%d,%d actual=%u,%u,%u,%u gl=0x%04x\n",
           name, pass ? "PASS" : "FAIL", r, g, b, a,
           pixel[0], pixel[1], pixel[2], pixel[3], error);
}

static void report_depth(const char *name, float expected, float tolerance) {
    float depth = -1.0f;
    glReadPixels(0, 0, 1, 1, GL_DEPTH_COMPONENT, GL_FLOAT, &depth);
    GLenum error = glGetError();
    int pass = error == GL_NO_ERROR && fabsf(depth - expected) <= tolerance;
    ++cases;
    if (!pass) ++failures;
    printf("CASE %-32s %s expected=%.4f actual=%.4f gl=0x%04x\n",
           name, pass ? "PASS" : "FAIL", expected, depth, error);
}

static void report_stencil(const char *name, unsigned expected) {
    unsigned stencil = 0xffffffffu;
    glReadPixels(0, 0, 1, 1, GL_STENCIL_INDEX, GL_UNSIGNED_INT, &stencil);
    GLenum error = glGetError();
    int pass = error == GL_NO_ERROR && stencil == expected;
    ++cases;
    if (!pass) ++failures;
    printf("CASE %-32s %s expected=%u actual=%u gl=0x%04x\n",
           name, pass ? "PASS" : "FAIL", expected, stencil, error);
}

static void draw(GLuint program, float depth, float r, float g, float b, float a) {
    glUseProgram(program);
    glUniform1f(glGetUniformLocation(program, "ndc_depth"), depth);
    glUniform4f(glGetUniformLocation(program, "draw_color"), r, g, b, a);
    glDrawArrays(GL_TRIANGLES, 0, 3);
}

static void draw_mrt(GLuint program, const float color0[4], const float color1[4]) {
    glUseProgram(program);
    glUniform4fv(glGetUniformLocation(program, "draw_color0"), 1, color0);
    glUniform4fv(glGetUniformLocation(program, "draw_color1"), 1, color1);
    glDrawArrays(GL_TRIANGLES, 0, 3);
}

static void reset_state(void) {
    glDisable(GL_BLEND);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_STENCIL_TEST);
    glDisable(GL_SCISSOR_TEST);
    glDisable(GL_CULL_FACE);
    glDepthMask(GL_TRUE);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
    glStencilMask(0xff);
    glStencilFunc(GL_ALWAYS, 0, 0xff);
    glStencilOp(GL_KEEP, GL_KEEP, GL_KEEP);
    glBlendEquationSeparate(GL_FUNC_ADD, GL_FUNC_ADD);
    glBlendFuncSeparate(GL_ONE, GL_ZERO, GL_ONE, GL_ZERO);
    glFrontFace(GL_CCW);
    glCullFace(GL_BACK);
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
        config_count < 1) {
        fprintf(stderr, "FATAL egl_config error=0x%x\n", eglGetError());
        return 2;
    }
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 4, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) {
        fprintf(stderr, "FATAL egl_context error=0x%x\n", eglGetError());
        return 2;
    }
    printf("INFO vendor=%s renderer=%s version=%s\n", glGetString(GL_VENDOR),
           glGetString(GL_RENDERER), glGetString(GL_VERSION));

    GLuint color = 0, depth_stencil = 0, framebuffer = 0, vao = 0;
    glGenTextures(1, &color);
    glBindTexture(GL_TEXTURE_2D, color);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenRenderbuffers(1, &depth_stencil);
    glBindRenderbuffer(GL_RENDERBUFFER, depth_stencil);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, 1, 1);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           color, 0);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT,
                              GL_RENDERBUFFER, depth_stencil);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "FATAL framebuffer_incomplete status=0x%x\n",
                glCheckFramebufferStatus(GL_FRAMEBUFFER));
        return 2;
    }
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glViewport(0, 0, 1, 1);
    GLuint program = make_program();
    GLuint mrt_program = make_mrt_program();

    reset_state();
    glClearColor(0, 0, 1, 1);
    glClearDepth(0.5);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    glEnable(GL_DEPTH_TEST);
    glDepthFunc(GL_LESS);
    draw(program, -0.5f, 1, 0, 0, 1);
    report_color("depth_less_pass_color", 255, 0, 0, 255, 0);
    report_depth("depth_less_pass_write", 0.25f, 0.002f);
    draw(program, 0.5f, 0, 1, 0, 1);
    report_color("depth_less_reject_color", 255, 0, 0, 255, 0);
    report_depth("depth_less_reject_depth", 0.25f, 0.002f);

    reset_state();
    glClearColor(0, 0, 1, 1);
    glClearDepth(0.5);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    glEnable(GL_DEPTH_TEST);
    glDepthFunc(GL_LESS);
    glDepthMask(GL_FALSE);
    draw(program, -0.5f, 1, 0, 0, 1);
    report_color("depth_mask_false_color", 255, 0, 0, 255, 0);
    report_depth("depth_mask_false_value", 0.5f, 0.002f);

    reset_state();
    glClearColor(0, 0, 1, 1);
    glClearDepth(0.25);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    glEnable(GL_DEPTH_TEST);
    glDepthFunc(GL_GREATER);
    draw(program, 0.5f, 0, 1, 0, 1);
    report_color("depth_greater_pass", 0, 255, 0, 255, 0);

    reset_state();
    glClearColor(0, 0, 1, 1);
    glClearStencil(0);
    glClear(GL_COLOR_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);
    glEnable(GL_STENCIL_TEST);
    glStencilFunc(GL_ALWAYS, 5, 0xff);
    glStencilOp(GL_KEEP, GL_KEEP, GL_REPLACE);
    draw(program, 0, 1, 0, 0, 1);
    report_stencil("stencil_replace", 5);
    glStencilFunc(GL_EQUAL, 4, 0xff);
    glStencilOp(GL_KEEP, GL_KEEP, GL_KEEP);
    draw(program, 0, 0, 1, 0, 1);
    report_color("stencil_equal_reject", 255, 0, 0, 255, 0);

    reset_state();
    glClearStencil(0xa0);
    glClear(GL_STENCIL_BUFFER_BIT);
    glEnable(GL_STENCIL_TEST);
    glStencilMask(0x0f);
    glStencilFunc(GL_ALWAYS, 0x05, 0xff);
    glStencilOp(GL_KEEP, GL_KEEP, GL_REPLACE);
    draw(program, 0, 1, 1, 1, 1);
    report_stencil("stencil_write_mask", 0xa5);

    reset_state();
    glClearColor(0, 0, 1, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_BLEND);
    glBlendEquation(GL_FUNC_ADD);
    glBlendFuncSeparate(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA, GL_ONE, GL_ZERO);
    draw(program, 0, 1, 0, 0, 0.25f);
    report_color("blend_src_alpha", 64, 0, 191, 64, 1);

    reset_state();
    glClearColor(0.2f, 0.2f, 0.2f, 0.2f);
    glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_BLEND);
    glBlendFunc(GL_ONE, GL_ONE);
    draw(program, 0, 0.3f, 0.3f, 0.3f, 0.3f);
    report_color("blend_additive", 128, 128, 128, 128, 1);

    reset_state();
    glClearColor(0.75f, 0.75f, 0.75f, 0.75f);
    glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_BLEND);
    glBlendEquation(GL_FUNC_REVERSE_SUBTRACT);
    glBlendFunc(GL_ONE, GL_ONE);
    draw(program, 0, 0.25f, 0.25f, 0.25f, 0.25f);
    report_color("blend_reverse_subtract", 128, 128, 128, 128, 1);

    reset_state();
    glClearColor(0, 0, 0, 0);
    glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_BLEND);
    glBlendColor(0.25f, 0.5f, 0.75f, 0.5f);
    glBlendFunc(GL_CONSTANT_COLOR, GL_ZERO);
    draw(program, 0, 1, 1, 1, 1);
    report_color("blend_constant_color", 64, 128, 191, 128, 1);

    reset_state();
    glClearColor(0.1f, 0.2f, 0.3f, 0.4f);
    glClear(GL_COLOR_BUFFER_BIT);
    glColorMask(GL_FALSE, GL_TRUE, GL_FALSE, GL_TRUE);
    draw(program, 0, 1, 0.5f, 1, 0.25f);
    report_color("color_write_mask", 26, 128, 76, 64, 1);

    reset_state();
    glClearColor(0, 0, 1, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_CULL_FACE);
    glFrontFace(GL_CCW);
    draw(program, 0, 1, 0, 0, 1);
    report_color("cull_ccw_front_kept", 255, 0, 0, 255, 0);
    glClearColor(0, 0, 1, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glFrontFace(GL_CW);
    draw(program, 0, 1, 0, 0, 1);
    report_color("cull_cw_front_rejected", 0, 0, 255, 255, 0);

    GLuint color1 = 0;
    glGenTextures(1, &color1);
    glBindTexture(GL_TEXTURE_2D, color1);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT1, GL_TEXTURE_2D,
                           color1, 0);
    const GLenum draw_buffers[] = {GL_COLOR_ATTACHMENT0, GL_COLOR_ATTACHMENT1};
    glDrawBuffers(2, draw_buffers);
    const float black[4] = {0, 0, 0, 0};
    const float blue[4] = {0, 0, 1, 1};
    const float red[4] = {1, 0, 0, 1};
    const float green[4] = {0, 1, 0, 1};

    reset_state();
    glClearBufferfv(GL_COLOR, 0, black);
    glClearBufferfv(GL_COLOR, 1, black);
    draw_mrt(mrt_program, red, green);
    glReadBuffer(GL_COLOR_ATTACHMENT0);
    report_color("mrt_output_location_0", 255, 0, 0, 255, 0);
    glReadBuffer(GL_COLOR_ATTACHMENT1);
    report_color("mrt_output_location_1", 0, 255, 0, 255, 0);

    reset_state();
    glClearBufferfv(GL_COLOR, 0, blue);
    glClearBufferfv(GL_COLOR, 1, blue);
    glColorMaski(1, GL_FALSE, GL_FALSE, GL_FALSE, GL_FALSE);
    draw_mrt(mrt_program, red, green);
    glReadBuffer(GL_COLOR_ATTACHMENT0);
    report_color("indexed_color_mask_target_0", 255, 0, 0, 255, 0);
    glReadBuffer(GL_COLOR_ATTACHMENT1);
    report_color("indexed_color_mask_target_1", 0, 0, 255, 255, 0);

    reset_state();
    glClearBufferfv(GL_COLOR, 0, black);
    glClearBufferfv(GL_COLOR, 1, blue);
    glEnablei(GL_BLEND, 1);
    glBlendEquationi(1, GL_FUNC_ADD);
    glBlendFunci(1, GL_ONE, GL_ONE);
    draw_mrt(mrt_program, red, green);
    glReadBuffer(GL_COLOR_ATTACHMENT0);
    report_color("indexed_blend_target_0", 255, 0, 0, 255, 0);
    glReadBuffer(GL_COLOR_ATTACHMENT1);
    report_color("indexed_blend_target_1", 0, 255, 255, 255, 0);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures, failures);
    return failures ? 1 : 0;
}
