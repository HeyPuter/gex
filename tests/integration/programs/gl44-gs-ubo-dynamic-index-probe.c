/* A dynamic index into a struct array inside a uniform block, read by an
 * emulated geometry shader, from an ordinary buffer and from a persistently
 * mapped one.
 *
 * The last structural difference between KHR-GL44.buffer_storage
 * .map_persistent_draw and the probes that clear it. Its geometry shader reads
 *
 *     layout (std140, binding = 0) uniform Rectangles {
 *         Rectangle rectangle[2];
 *     } rectangles;
 *     ... rectangles.rectangle[vs_gs_index[0]].position.x ...
 *
 * which is not a block array -- that is gl44-gs-block-array-probe, and it
 * passes -- but a single block holding an array of structs, indexed at run
 * time. That is a dynamic *offset* within one block rather than a dynamic
 * block number, and the two take different paths.
 *
 * The value is carried out through the colour rather than through the geometry
 * it positions, so a wrong element is a wrong number instead of a misplaced
 * quad. Element 0 holds 64 and element 1 holds 192, so 64 means the index was
 * ignored, 192 means it worked, and anything else means the offset arithmetic
 * is wrong.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-40s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 440 core\n"
    "out uint vs_gs_index;\n"
    "void main() { vs_gs_index = uint(gl_VertexID);\n"
    "              gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
static const char *gs_src =
    "#version 440 core\n"
    "layout(points)                           in;\n"
    "layout(triangle_strip, max_vertices = 4) out;\n"
    "struct Rectangle {\n"
    "    vec2 position;\n"
    "    vec2 size;\n"
    "};\n"
    "layout (std140, binding = 0) uniform Rectangles {\n"
    "    Rectangle rectangle[2];\n"
    "} rectangles;\n"
    "in  uint  vs_gs_index[];\n"
    "out float gs_fs_color;\n"
    "void main() {\n"
    "    const float value = rectangles.rectangle[vs_gs_index[0]].position.x;\n"
    "    gs_fs_color = value;\n"
    "    gl_Position = vec4(-1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4(-1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4( 1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4( 1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
    "}\n";
static const char *fs_src =
    "#version 440 core\n"
    "in  float gs_fs_color;\n"
    "out vec4  fs_out_color;\n"
    "void main() { fs_out_color = vec4(gs_fs_color / 255.0, 0, 0, 1); }\n";

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2048] = {0};
               glGetShaderInfoLog(sh, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s compile failed: %s\n", what, log); }
    return ok ? sh : 0;
}

/* std140: Rectangle is two vec2s, which pack into one vec4 each having
   alignment 8, so the struct is 16 bytes and the array element stride is 16. */
static void fill(float *block) {
    block[0] = 64.0f;  block[1] = 0.0f;   /* rectangle[0].position */
    block[2] = 1.0f;   block[3] = 1.0f;   /* rectangle[0].size */
    block[4] = 192.0f; block[5] = 0.0f;   /* rectangle[1].position */
    block[6] = 1.0f;   block[7] = 1.0f;   /* rectangle[1].size */
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint vs = compile(GL_VERTEX_SHADER, vs_src, "vs");
    GLuint gs = compile(GL_GEOMETRY_SHADER, gs_src, "gs");
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !gs || !fs) { printf("SUMMARY cases=0 passed=0 failed=0\n");
                            return 3; }
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, gs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[2048] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log); }
    eq("dynamic_struct_index_links", linked, 1);
    if (!linked) { printf("SUMMARY cases=%d passed=%d failed=%d\n", cases,
                          passed, cases - passed); return 1; }

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glUseProgram(program);
    glViewport(0, 0, 16, 16);

    /* First from an ordinary buffer, which isolates the dynamic index from
       anything to do with persistence. */
    float block[8]; fill(block);
    GLuint ubo = 0; glGenBuffers(1, &ubo);
    glBindBuffer(GL_UNIFORM_BUFFER, ubo);
    glBufferData(GL_UNIFORM_BUFFER, sizeof block, block, GL_STATIC_DRAW);
    glBindBufferBase(GL_UNIFORM_BUFFER, 0, ubo);
    for (unsigned element = 0; element < 2; ++element) {
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f); glClear(GL_COLOR_BUFFER_BIT);
        /* One point, whose gl_VertexID selects the element. */
        glDrawArrays(GL_POINTS, (GLint)element, 1);
        glMemoryBarrier(GL_ALL_BARRIER_BITS);
        unsigned char pixel[4] = {0};
        glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
        char name[64];
        snprintf(name, sizeof name, "plain_ubo_element_%u", element);
        eq(name, pixel[0], element == 0 ? 64 : 192);
    }

    /* Then from a persistently mapped, immutable buffer written through the
       mapping -- the CTS's own arrangement for this block. */
    GLuint pbuf = 0; glGenBuffers(1, &pbuf);
    glBindBuffer(GL_UNIFORM_BUFFER, pbuf);
    glBufferStorage(GL_UNIFORM_BUFFER, sizeof block, NULL,
                    GL_MAP_PERSISTENT_BIT | GL_MAP_WRITE_BIT);
    eq("persistent_ubo_storage_is_clean", glGetError(), GL_NO_ERROR);
    float *map = (float *)glMapBufferRange(
        GL_UNIFORM_BUFFER, 0, sizeof block,
        GL_MAP_PERSISTENT_BIT | GL_MAP_WRITE_BIT |
        GL_MAP_FLUSH_EXPLICIT_BIT);
    eq("persistent_ubo_map_succeeded", map != NULL, 1);
    if (map) {
        fill(map);
        glFlushMappedBufferRange(GL_UNIFORM_BUFFER, 0, sizeof block);
        glBindBufferBase(GL_UNIFORM_BUFFER, 0, pbuf);
        for (unsigned element = 0; element < 2; ++element) {
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_POINTS, (GLint)element, 1);
            glMemoryBarrier(GL_ALL_BARRIER_BITS);
            unsigned char pixel[4] = {0};
            glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
            char name[64];
            snprintf(name, sizeof name, "persistent_ubo_element_%u", element);
            eq(name, pixel[0], element == 0 ? 64 : 192);
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
