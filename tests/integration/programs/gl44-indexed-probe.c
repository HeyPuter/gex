#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>

#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "indexed query failed: %s (line %d)\n", message, __LINE__); return __LINE__; } \
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
        fprintf(stderr, "indexed shader compile failed: %s\n", log);
        exit(2);
    }
    return shader;
}

/* Records the case and carries on.
 *
 * This used to exit(3) on the first failure, and the fourth-from-last case
 * fails on llvmpipe as well as here -- so the three instanced base-vertex /
 * base-instance draws after it, which are the GL 4.2 and 4.4 features this
 * probe exists to cover, had never run on *either* driver. A fail-fast probe
 * turns one defect into a blind spot behind it. */
static int cases;
static int passed;

static void check_green(const char *operation) {
    unsigned char pixel[4] = {0, 0, 0, 0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    GLenum error = glGetError();
    const int ok = error == GL_NO_ERROR && pixel[0] <= 2 && pixel[1] >= 253 &&
                   pixel[2] <= 2 && pixel[3] >= 253;
    ++cases;
    if (ok) {
        ++passed;
    } else {
        fprintf(stderr, "INFO %s error=0x%x pixel=%u,%u,%u,%u\n",
                operation, error, pixel[0], pixel[1], pixel[2], pixel[3]);
    }
    printf("CASE %s %s\n", operation, ok ? "PASS" : "FAIL");
}

static void begin_draw(void) {
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    EGLConfig config = NULL; EGLint config_count = 0;
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

    /* Consume a name in another namespace. A binding query must return the
       guest's buffer name, not MobileGL's independently allocated name. */
    GLuint shader = glCreateShader(GL_VERTEX_SHADER);
    CHECK(shader != 0, "shader name allocation");

    GLuint buffer = 0;
    glGenBuffers(1, &buffer);
    CHECK(buffer != 0, "buffer name allocation");
    /* Only the guest allocates object names from one global pool, so this is
     * a property of the transport and not of GL: a native driver is entitled
     * to hand out shader 1 and buffer 1, and llvmpipe and this driver both
     * do. The condition used to be "not the reference run", which the native
     * reference set and the *native driver* run did not -- so a guest-only
     * invariant was asserted against a native driver and stopped the probe at
     * its third case, hiding the six after it on that driver entirely. Opt in
     * from the guest instead, so both native runs skip it. */
    if (getenv("GEX_GUEST_NAMES"))
        CHECK(buffer != shader, "global guest name allocation");
    glBindBuffer(GL_UNIFORM_BUFFER, buffer);
    glBufferData(GL_UNIFORM_BUFFER, 1024, NULL, GL_DYNAMIC_DRAW);
    glBindBufferRange(GL_UNIFORM_BUFFER, 3, buffer, 256, 128);
    glBindBufferRange(GL_SHADER_STORAGE_BUFFER, 5, buffer, 512, 192);

    GLint value = -1;
    GLint64 value64 = -1;
    glGetIntegeri_v(GL_UNIFORM_BUFFER_BINDING, 3, &value);
    CHECK((GLuint)value == buffer, "uniform binding returns guest object name");
    glGetInteger64i_v(GL_UNIFORM_BUFFER_START, 3, &value64);
    CHECK(value64 == 256, "uniform range start");
    glGetInteger64i_v(GL_UNIFORM_BUFFER_SIZE, 3, &value64);
    CHECK(value64 == 128, "uniform range size");

    glGetIntegeri_v(GL_SHADER_STORAGE_BUFFER_BINDING, 5, &value);
    CHECK((GLuint)value == buffer, "storage binding returns guest object name");
    glGetIntegeri_v(GL_SHADER_STORAGE_BUFFER_START, 5, &value);
    CHECK(value == 512, "storage range start");
    glGetIntegeri_v(GL_SHADER_STORAGE_BUFFER_SIZE, 5, &value);
    CHECK(value == 192, "storage range size");

    const GLint minimum_count[3] = {65535, 65535, 65535};
    const GLint minimum_size[3] = {1024, 1024, 64};
    for (GLuint axis = 0; axis < 3; ++axis) {
        glGetIntegeri_v(GL_MAX_COMPUTE_WORK_GROUP_COUNT, axis, &value);
        CHECK(value >= minimum_count[axis], "OpenGL compute group-count minimum");
        glGetIntegeri_v(GL_MAX_COMPUTE_WORK_GROUP_SIZE, axis, &value);
        CHECK(value >= minimum_size[axis], "OpenGL compute group-size minimum");
    }

    const char *vertex_source =
        "#version 420 core\n"
        "layout(location=0) in vec2 position;\n"
        "layout(location=1) in float enabled_instance;\n"
        "void main() { gl_Position = vec4(position * enabled_instance, 0.0, 1.0); }\n";
    const char *fragment_source =
        "#version 420 core\n"
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

    /* Vertices 0..2 are deliberately degenerate. A base-vertex error therefore
       leaves the pixel black instead of accidentally drawing plausible data. */
    const float vertices[] = {
        0, 0, 0, 0, 0, 0,
        -1, -1, 3, -1, -1, 3,
    };
    const unsigned short indices[] = {0, 1, 2, 3, 4, 5};
    const float instances[] = {0, 0, 1};
    GLuint vao = 0, vertex_buffer = 0, index_buffer = 0, instance_buffer = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &vertex_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, vertex_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
    glGenBuffers(1, &instance_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, instance_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(instances), instances, GL_STATIC_DRAW);
    glEnableVertexAttribArray(1);
    glVertexAttribPointer(1, 1, GL_FLOAT, GL_FALSE, 0, 0);
    glVertexAttribDivisor(1, 1);
    glGenBuffers(1, &index_buffer);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, index_buffer);
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(indices), indices, GL_STATIC_DRAW);
    GLuint color_texture = 0, framebuffer = 0;
    glGenTextures(1, &color_texture);
    glBindTexture(GL_TEXTURE_2D, color_texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           color_texture, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "draw framebuffer completeness");
    glViewport(0, 0, 1, 1);

    /* Non-instanced calls use the generic attribute value one. */
    glDisableVertexAttribArray(1);
    glVertexAttrib1f(1, 1.0f);
    begin_draw();
    glDrawElementsBaseVertex(GL_TRIANGLES, 3, GL_UNSIGNED_SHORT, 0, 3);
    check_green("glDrawElementsBaseVertex");
    begin_draw();
    glDrawRangeElements(GL_TRIANGLES, 3, 5, 3, GL_UNSIGNED_SHORT,
                        (const void *)(3 * sizeof(unsigned short)));
    check_green("glDrawRangeElements offset");
    begin_draw();
    glDrawRangeElementsBaseVertex(GL_TRIANGLES, 0, 2, 3, GL_UNSIGNED_SHORT, 0, 3);
    check_green("glDrawRangeElementsBaseVertex");
    const GLsizei multi_counts[] = {0, 3};
    const void *multi_offsets[] = {0, (const void *)(3 * sizeof(unsigned short))};
    begin_draw();
    glMultiDrawElements(GL_TRIANGLES, multi_counts, GL_UNSIGNED_SHORT,
                        multi_offsets, 2);
    check_green("glMultiDrawElements offset array");
    const void *multi_base_offsets[] = {0, 0};
    const GLint multi_bases[] = {0, 3};
    begin_draw();
    glMultiDrawElementsBaseVertex(GL_TRIANGLES, multi_counts, GL_UNSIGNED_SHORT,
                                  multi_base_offsets, 2, multi_bases);
    check_green("glMultiDrawElementsBaseVertex");

    /* A single instance starting at two must fetch instances[2], not [0]. */
    glEnableVertexAttribArray(1);
    begin_draw();
    glDrawElementsInstancedBaseInstance(GL_TRIANGLES, 3, GL_UNSIGNED_SHORT,
                                        (const void *)(3 * sizeof(unsigned short)), 1, 2);
    check_green("glDrawElementsInstancedBaseInstance");
    glDisableVertexAttribArray(1);
    glVertexAttrib1f(1, 1.0f);
    begin_draw();
    glDrawElementsInstancedBaseVertex(GL_TRIANGLES, 3, GL_UNSIGNED_SHORT, 0, 1, 3);
    check_green("glDrawElementsInstancedBaseVertex");
    glEnableVertexAttribArray(1);
    begin_draw();
    glDrawElementsInstancedBaseVertexBaseInstance(GL_TRIANGLES, 3, GL_UNSIGNED_SHORT,
                                                  0, 1, 3, 2);
    check_green("glDrawElementsInstancedBaseVertexBaseInstance");

    CHECK(glGetError() == GL_NO_ERROR, "unexpected GL error");
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
