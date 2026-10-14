/* Client-memory vertex arrays: the guest sends each one to the host with its
 * pointer command (CLIENT_ARRAY), or, with GEXGL_CLIENT_ARRAYS=buffer, copies
 * it into a scratch buffer of its own first. Either way what is drawn must be
 * what GL draws from client memory, including the cases that catch a copy
 * that starts in the wrong place or keeps stale bytes:
 *
 *   - glDrawElements whose smallest index is not 0, from interleaved arrays
 *     (the copy has to start at element 0, not at the smallest index);
 *   - glDrawArrays with first > 0;
 *   - one draw with an attribute from a buffer object and another from
 *     client memory, fixed-function and generic, and the application's
 *     GL_ARRAY_BUFFER binding unchanged afterwards;
 *   - the same client pointer drawn twice with the bytes changed between
 *     (the second draw must see the new bytes).
 *
 * Each case clears a 16x16 pbuffer to black, draws a quad over all of it and
 * reads the centre pixel back.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

/* Compatibility-profile entry points glcorearb.h leaves out. */
extern void glVertexPointer(GLint, GLenum, GLsizei, const void *);
extern void glColorPointer(GLint, GLenum, GLsizei, const void *);
extern void glEnableClientState(GLenum);
extern void glDisableClientState(GLenum);
#define GL_VERTEX_ARRAY 0x8074
#define GL_COLOR_ARRAY 0x8076

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static int no_error(const char *name) {
    const GLenum error = glGetError();
    if (error != GL_NO_ERROR)
        fprintf(stderr, "INFO %s err=0x%04x\n", name, (unsigned)error);
    return error == GL_NO_ERROR;
}

/* The centre pixel is (r,g,b) to within a step of 8-bit rounding. */
static int centre_is(const char *name, int r, int g, int b) {
    unsigned char px[4] = {0, 0, 0, 0};
    glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    const int ok = px[0] >= r - 2 && px[0] <= r + 2 && px[1] >= g - 2 &&
                   px[1] <= g + 2 && px[2] >= b - 2 && px[2] <= b + 2;
    if (!ok)
        fprintf(stderr, "INFO %s centre=%u,%u,%u want %d,%d,%d\n", name,
                px[0], px[1], px[2], r, g, b);
    return ok && no_error(name);
}

/* Interleaved: position (2 floats) then colour (4 unsigned bytes). */
struct vertex {
    float x, y;
    unsigned char rgba[4];
};

static void quad(struct vertex *v, unsigned char r, unsigned char g,
                 unsigned char b) {
    static const float corner[4][2] = {{-1, -1}, {1, -1}, {1, 1}, {-1, 1}};
    for (int i = 0; i < 4; ++i) {
        v[i].x = corner[i][0];
        v[i].y = corner[i][1];
        v[i].rgba[0] = r; v[i].rgba[1] = g; v[i].rgba[2] = b; v[i].rgba[3] = 255;
    }
}

static GLuint shader(GLenum kind, const char *source) {
    GLuint s = glCreateShader(kind);
    glShaderSource(s, 1, &source, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[512] = "";
        glGetShaderInfoLog(s, sizeof log, NULL, log);
        fprintf(stderr, "INFO shader: %s\n", log);
    }
    return s;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
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
    /* Client arrays and fixed function exist only in a compatibility profile. */
    const EGLint context_attributes[] = {
        0x30FD /* EGL_CONTEXT_OPENGL_PROFILE_MASK */, 0x2, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    while (glGetError() != GL_NO_ERROR) { }
    glViewport(0, 0, 16, 16);
    glClearColor(0, 0, 0, 1);

    /* Eight vertices: 0-3 a red quad that must not be drawn, 4-7 the one
       that is. Indices 4..7 only, so the smallest index is 4. */
    struct vertex v[8];
    quad(v, 255, 0, 0);
    quad(v + 4, 0, 255, 0);
    static const GLushort upper[6] = {4, 5, 6, 4, 6, 7};

    /* Fixed function, both arrays client-side and interleaved. */
    glEnableClientState(GL_VERTEX_ARRAY);
    glEnableClientState(GL_COLOR_ARRAY);
    glClear(GL_COLOR_BUFFER_BIT);
    glVertexPointer(2, GL_FLOAT, sizeof v[0], &v[0].x);
    glColorPointer(4, GL_UNSIGNED_BYTE, sizeof v[0], v[0].rgba);
    glDrawElements(GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, upper);
    result("ff-elements-min-index", centre_is("ff-elements-min-index", 0, 255, 0));

    /* The same pointers, the bytes changed: the draw sees the new ones. */
    quad(v + 4, 0, 0, 255);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawElements(GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, upper);
    result("ff-bytes-changed", centre_is("ff-bytes-changed", 0, 0, 255));

    /* glDrawArrays from vertex 4 (a fan of 4..7). */
    quad(v + 4, 255, 255, 0);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_FAN, 4, 4);
    result("ff-arrays-first", centre_is("ff-arrays-first", 255, 255, 0));

    /* Positions from a buffer object, colours from client memory; the
       application's GL_ARRAY_BUFFER stays bound. */
    GLuint vbo = 0;
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof v, v, GL_STATIC_DRAW);
    glVertexPointer(2, GL_FLOAT, sizeof v[0], (const void *)0);
    glBindBuffer(GL_ARRAY_BUFFER, 0);
    unsigned char colours[8][4];
    memset(colours, 0, sizeof colours);
    for (int i = 4; i < 8; ++i) { colours[i][0] = 255; colours[i][2] = 255; colours[i][3] = 255; }
    glColorPointer(4, GL_UNSIGNED_BYTE, 0, colours);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawElements(GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, upper);
    GLint bound = -1;
    glGetIntegerv(GL_ARRAY_BUFFER_BINDING, &bound);
    result("ff-mixed-vbo-client", centre_is("ff-mixed-vbo-client", 255, 0, 255) &&
                                  bound == (GLint)vbo);
    if (bound != (GLint)vbo) fprintf(stderr, "INFO ff-mixed array buffer %d want %u\n", bound, vbo);
    glBindBuffer(GL_ARRAY_BUFFER, 0);
    glDisableClientState(GL_VERTEX_ARRAY);
    glDisableClientState(GL_COLOR_ARRAY);

    /* Generic attributes. */
    const GLuint vs = shader(GL_VERTEX_SHADER,
        "#version 120\n"
        "attribute vec2 pos; attribute vec4 colour; varying vec4 c;\n"
        "void main() { c = colour; gl_Position = vec4(pos, 0.0, 1.0); }\n");
    const GLuint fs = shader(GL_FRAGMENT_SHADER,
        "#version 120\n"
        "varying vec4 c; void main() { gl_FragColor = c; }\n");
    const GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    glAttachShader(program, fs);
    glBindAttribLocation(program, 0, "pos");
    glBindAttribLocation(program, 3, "colour");
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) fprintf(stderr, "INFO program did not link\n");
    glUseProgram(program);
    glEnableVertexAttribArray(0);
    glEnableVertexAttribArray(3);

    quad(v + 4, 0, 255, 255);
    glClear(GL_COLOR_BUFFER_BIT);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, sizeof v[0], &v[0].x);
    glVertexAttribPointer(3, 4, GL_UNSIGNED_BYTE, GL_TRUE, sizeof v[0], v[0].rgba);
    glDrawElements(GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, upper);
    result("attrib-elements-min-index", linked && centre_is("attrib-elements-min-index", 0, 255, 255));

    quad(v + 4, 128, 128, 128);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_FAN, 4, 4);
    result("attrib-arrays-first", linked && centre_is("attrib-arrays-first", 128, 128, 128));

    /* Position from the buffer object (vertices 0-3 there are red, but only
       the position is read), colour from client memory. */
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, sizeof v[0], (const void *)0);
    glBindBuffer(GL_ARRAY_BUFFER, 0);
    for (int i = 4; i < 8; ++i) { colours[i][0] = 255; colours[i][1] = 128; colours[i][2] = 0; }
    glVertexAttribPointer(3, 4, GL_UNSIGNED_BYTE, GL_TRUE, 0, colours);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawElements(GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, upper);
    bound = -1;
    glGetIntegerv(GL_ARRAY_BUFFER_BINDING, &bound);
    result("attrib-mixed-vbo-client", linked && centre_is("attrib-mixed-vbo-client", 255, 128, 0) &&
                                      bound == (GLint)vbo);
    if (bound != (GLint)vbo) fprintf(stderr, "INFO attrib-mixed array buffer %d want %u\n", bound, vbo);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
