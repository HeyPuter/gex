/* Depth textures on targets other than plain 2D.
 *
 * A cascaded shadow map is a 2D array of depth, and an omnidirectional shadow
 * map is a depth cube -- both are how real engines light a scene, and WGSL has
 * `texture_depth_2d_array` and `texture_depth_cube` for exactly them. The
 * driver's is_format_supported used to answer "no depth format exists" for
 * anything but PIPE_TEXTURE_2D, and st/mesa has no fallback for that: it
 * asserts inside _mesa_choose_texture_format. This probe is what stands behind
 * widening it, since a capability claim with no passing test behind it is the
 * one thing this driver does not do.
 *
 * Each case renders a known depth into one layer or face and then compares
 * against a reference in front of and behind it, so a lookup that reads the
 * wrong layer -- or ignores the layer entirely -- fails rather than passing by
 * luck: the two layers hold depths on opposite sides of the same reference.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vertex_source =
    "#version 330 core\n"
    "in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

/* The depth written into a layer, chosen per draw. */
static const char *fill_vertex_source =
    "#version 330 core\n"
    "in vec2 pos;\n"
    "uniform float depth;\n"
    "void main() { gl_Position = vec4(pos, depth, 1.0); }\n";
static const char *fill_fragment_source =
    "#version 330 core\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = vec4(1.0); }\n";

/* layer and ref come from uniforms so one program serves every case. */
static const char *array_shadow_source =
    "#version 330 core\n"
    "uniform sampler2DArrayShadow shadowmap;\n"
    "uniform float layer;\n"
    "uniform float ref;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    fragColour = vec4(texture(shadowmap, vec4(0.5, 0.5, layer, ref)));\n"
    "}\n";

static const char *cube_shadow_source =
    "#version 330 core\n"
    "uniform samplerCubeShadow shadowmap;\n"
    "uniform vec3 dir;\n"
    "uniform float ref;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    fragColour = vec4(texture(shadowmap, vec4(dir, ref)));\n"
    "}\n";

static GLuint compile(GLenum stage, const char *source, const char *name) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s compile failed: %s\n", name, log);
        return 0;
    }
    return shader;
}

static GLuint link(GLuint vertex, const char *fragment_source,
                   const char *name) {
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source, name);
    if (!fragment) return 0;
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[512] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s link failed: %s\n", name, log);
        return 0;
    }
    return program;
}

static GLuint colour_target;
static GLuint colour_fbo;
static GLuint position_buffer;

/* Draw the full viewport with `program` and read the one pixel back. */
static unsigned char draw_and_read(GLuint program) {
    glBindFramebuffer(GL_FRAMEBUFFER, colour_fbo);
    glViewport(0, 0, 1, 1);
    glUseProgram(program);
    const GLint position = glGetAttribLocation(program, "pos");
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glEnableVertexAttribArray(position);
    glVertexAttribPointer(position, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glClearColor(0.5f, 0.5f, 0.5f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned char got[4] = {0xAB, 0xAB, 0xAB, 0xAB};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
    return got[0];
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    glGenTextures(1, &colour_target);
    glBindTexture(GL_TEXTURE_2D, colour_target);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &colour_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, colour_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour_target, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "colour framebuffer incomplete\n");
        return 2;
    }

    static const float quad[8] = {
        -1.0f, -1.0f,  3.0f, -1.0f,  -1.0f, 3.0f,  3.0f, 3.0f,
    };
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source, "vertex");
    GLuint fill_vertex =
        compile(GL_VERTEX_SHADER, fill_vertex_source, "fill_vertex");
    if (!vertex || !fill_vertex) return 2;
    GLuint fill = 0;
    {
        GLuint fill_fs = compile(GL_FRAGMENT_SHADER, fill_fragment_source,
                                 "fill_fragment");
        if (!fill_fs) return 2;
        fill = glCreateProgram();
        glAttachShader(fill, fill_vertex);
        glAttachShader(fill, fill_fs);
        glLinkProgram(fill);
        GLint linked = 0;
        glGetProgramiv(fill, GL_LINK_STATUS, &linked);
        if (!linked) return 2;
    }

    /* ---- a two-layer depth array, one layer either side of ref 0.5 ---- */
    GLuint array_texture = 0, array_fbo = 0;
    glGenTextures(1, &array_texture);
    glBindTexture(GL_TEXTURE_2D_ARRAY, array_texture);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MAX_LEVEL, 0);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_COMPARE_MODE,
                    GL_COMPARE_REF_TO_TEXTURE);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_COMPARE_FUNC, GL_LEQUAL);
    glTexImage3D(GL_TEXTURE_2D_ARRAY, 0, GL_DEPTH_COMPONENT24, 2, 2, 2, 0,
                 GL_DEPTH_COMPONENT, GL_FLOAT, NULL);
    result("depth_array_allocated", glGetError() == GL_NO_ERROR);

    glGenFramebuffers(1, &array_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, array_fbo);
    glDrawBuffer(GL_NONE);
    int array_ok = 1;
    /* NDC 0.4 is window depth 0.7; NDC -0.4 is 0.3. Reference 0.5 is behind
     * the first and in front of the second. */
    static const float layer_ndc[2] = {0.4f, -0.4f};
    for (int layer = 0; layer < 2 && array_ok; ++layer) {
        glFramebufferTextureLayer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT,
                                  array_texture, 0, layer);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) !=
            GL_FRAMEBUFFER_COMPLETE) {
            fprintf(stderr, "INFO depth array layer %d incomplete\n", layer);
            array_ok = 0;
            break;
        }
        glViewport(0, 0, 2, 2);
        glEnable(GL_DEPTH_TEST);
        glDepthFunc(GL_ALWAYS);
        glDepthMask(GL_TRUE);
        glClearDepth(1.0);
        glClear(GL_DEPTH_BUFFER_BIT);
        glUseProgram(fill);
        glUniform1f(glGetUniformLocation(fill, "depth"), layer_ndc[layer]);
        const GLint position = glGetAttribLocation(fill, "pos");
        glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
        glEnableVertexAttribArray(position);
        glVertexAttribPointer(position, 2, GL_FLOAT, GL_FALSE, 0, NULL);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glDisable(GL_DEPTH_TEST);
        if (glGetError() != GL_NO_ERROR) array_ok = 0;
    }
    result("depth_array_rendered", array_ok);

    if (array_ok) {
        GLuint program = link(vertex, array_shadow_source, "array_shadow");
        if (!program) {
            result("depth_array_layer0", 0);
            result("depth_array_layer1", 0);
        } else {
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D_ARRAY, array_texture);
            glUseProgram(program);
            glUniform1i(glGetUniformLocation(program, "shadowmap"), 0);
            glUniform1f(glGetUniformLocation(program, "ref"), 0.5f);
            /* Layer 0 holds 0.7, so 0.5 <= 0.7 is true: lit. */
            glUniform1f(glGetUniformLocation(program, "layer"), 0.0f);
            unsigned char got0 = draw_and_read(program);
            /* Layer 1 holds 0.3, so 0.5 <= 0.3 is false: shadowed. */
            glUniform1f(glGetUniformLocation(program, "layer"), 1.0f);
            unsigned char got1 = draw_and_read(program);
            const GLenum error = glGetError();
            fprintf(stderr, "INFO depth_array layer0=%u layer1=%u "
                    "want=255,0 err=0x%04x\n", got0, got1, (unsigned)error);
            result("depth_array_layer0", error == GL_NO_ERROR && got0 >= 253);
            result("depth_array_layer1", error == GL_NO_ERROR && got1 <= 2);
        }
    } else {
        result("depth_array_layer0", 0);
        result("depth_array_layer1", 0);
    }

    /* ---- a depth cube map, +X rendered, -X left at the clear ---- */
    GLuint cube_texture = 0, cube_fbo = 0;
    glGenTextures(1, &cube_texture);
    glBindTexture(GL_TEXTURE_CUBE_MAP, cube_texture);
    glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_MAX_LEVEL, 0);
    glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_COMPARE_MODE,
                    GL_COMPARE_REF_TO_TEXTURE);
    glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_COMPARE_FUNC, GL_LEQUAL);
    for (int face = 0; face < 6; ++face)
        glTexImage2D(GL_TEXTURE_CUBE_MAP_POSITIVE_X + face, 0,
                     GL_DEPTH_COMPONENT24, 2, 2, 0, GL_DEPTH_COMPONENT,
                     GL_FLOAT, NULL);
    result("depth_cube_allocated", glGetError() == GL_NO_ERROR);

    glGenFramebuffers(1, &cube_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, cube_fbo);
    glDrawBuffer(GL_NONE);
    int cube_ok = 1;
    for (int face = 0; face < 6 && cube_ok; ++face) {
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT,
                               GL_TEXTURE_CUBE_MAP_POSITIVE_X + face,
                               cube_texture, 0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) !=
            GL_FRAMEBUFFER_COMPLETE) {
            fprintf(stderr, "INFO depth cube face %d incomplete\n", face);
            cube_ok = 0;
            break;
        }
        glViewport(0, 0, 2, 2);
        glEnable(GL_DEPTH_TEST);
        glDepthFunc(GL_ALWAYS);
        glDepthMask(GL_TRUE);
        glClearDepth(1.0);
        glClear(GL_DEPTH_BUFFER_BIT);
        glUseProgram(fill);
        /* +X gets 0.7 and every other face 0.3, so a lookup that samples the
         * wrong face answers the opposite way. */
        glUniform1f(glGetUniformLocation(fill, "depth"),
                    face == 0 ? 0.4f : -0.4f);
        const GLint position = glGetAttribLocation(fill, "pos");
        glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
        glEnableVertexAttribArray(position);
        glVertexAttribPointer(position, 2, GL_FLOAT, GL_FALSE, 0, NULL);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glDisable(GL_DEPTH_TEST);
        if (glGetError() != GL_NO_ERROR) cube_ok = 0;
    }
    result("depth_cube_rendered", cube_ok);

    if (cube_ok) {
        GLuint program = link(vertex, cube_shadow_source, "cube_shadow");
        if (!program) {
            result("depth_cube_plus_x", 0);
            result("depth_cube_minus_x", 0);
        } else {
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_CUBE_MAP, cube_texture);
            glUseProgram(program);
            glUniform1i(glGetUniformLocation(program, "shadowmap"), 0);
            glUniform1f(glGetUniformLocation(program, "ref"), 0.5f);
            glUniform3f(glGetUniformLocation(program, "dir"), 1.0f, 0.0f, 0.0f);
            unsigned char plus_x = draw_and_read(program);
            glUniform3f(glGetUniformLocation(program, "dir"), -1.0f, 0.0f,
                        0.0f);
            unsigned char minus_x = draw_and_read(program);
            const GLenum error = glGetError();
            fprintf(stderr, "INFO depth_cube +x=%u -x=%u want=255,0 "
                    "err=0x%04x\n", plus_x, minus_x, (unsigned)error);
            result("depth_cube_plus_x", error == GL_NO_ERROR && plus_x >= 253);
            result("depth_cube_minus_x", error == GL_NO_ERROR && minus_x <= 2);
        }
    } else {
        result("depth_cube_plus_x", 0);
        result("depth_cube_minus_x", 0);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
