/* glDrawElements with GL_UNSIGNED_BYTE, which WebGPU has no index width for.
 *
 * The driver widens the indices, so the thing worth testing is whether it
 * widens the *right bytes*. A test that drew triangle 0,1,2 would pass even if
 * the indices were misread, because vertex 0 is where a confused fetch tends
 * to land. So the buffer puts eight vertices in it, the first five collapsed
 * onto a single point where they cover nothing, and the triangle that covers
 * anything is 5,6,7. Reading the wrong bytes draws nothing.
 *
 * The second case asks for the same triangle through glDrawElementsBaseVertex
 * with indices 0,1,2 and a base vertex of 5. That exercises the other half of
 * the widening: the base vertex has to be folded into the indices, because a
 * biased byte index need not fit sixteen bits and the driver refuses to be
 * handed one separately.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
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
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

static const char *fragment_source =
    "#version 330 core\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = vec4(1.0); }\n";

static GLuint compile(GLenum stage, const char *source) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "shader compile failed: %s\n", log);
        return 0;
    }
    return shader;
}

#define SIDE 32

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
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

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) return 2;
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) return 2;

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);

    /* Vertices 0-4 sit on one point and cover nothing. 5, 6 and 7 are the
     * upper-right half of the target. */
    static const float vertices[16] = {
        -1.0f, -1.0f,  -1.0f, -1.0f,  -1.0f, -1.0f,  -1.0f, -1.0f,
        -1.0f, -1.0f,
         1.0f, -1.0f,   1.0f,  1.0f,  -1.0f,  1.0f,
    };
    GLuint vao = 0, vbo = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    static const GLubyte high[3] = {5, 6, 7};
    static const GLubyte low[3] = {0, 1, 2};
    GLuint ibo_high = 0, ibo_low = 0;
    glGenBuffers(1, &ibo_high);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo_high);
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(high), high, GL_STATIC_DRAW);
    glGenBuffers(1, &ibo_low);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo_low);
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(low), low, GL_STATIC_DRAW);
    glUseProgram(program);

    /* Inside the upper-right triangle, and inside the lower-left half that it
     * does not cover. */
    const int in_x = SIDE * 3 / 4, in_y = SIDE * 3 / 4;
    const int out_x = SIDE / 5, out_y = SIDE / 5;

    for (unsigned i = 0; i < 2; ++i) {
        const char *name = i == 0 ? "byte_indices" : "byte_indices_base_vertex";
        glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, i == 0 ? ibo_high : ibo_low);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        if (i == 0)
            glDrawElements(GL_TRIANGLES, 3, GL_UNSIGNED_BYTE, NULL);
        else
            glDrawElementsBaseVertex(GL_TRIANGLES, 3, GL_UNSIGNED_BYTE, NULL,
                                     5);
        unsigned char inside[4] = {0}, outside[4] = {0};
        glReadPixels(in_x, in_y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, inside);
        glReadPixels(out_x, out_y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, outside);
        const GLenum error = glGetError();
        const int ok = error == GL_NO_ERROR && inside[0] > 128 &&
                       outside[0] < 128;
        fprintf(stderr, "INFO %s inside=%u outside=%u err=0x%04x\n", name,
                inside[0], outside[0], (unsigned)error);
        result(name, ok);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
