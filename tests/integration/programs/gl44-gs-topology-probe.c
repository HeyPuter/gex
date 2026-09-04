/* What a geometry shader may take and emit, and how the draw may be issued.
 *
 * This driver has no geometry stage: a geometry shader runs as a compute pass
 * that writes vertices and indices for an ordinary indexed draw. That path
 * used to accept exactly one shape -- points or triangles in, points or a
 * triangle strip out, from a non-indexed, non-instanced draw of the shader's
 * own topology -- and refuse everything else at link time, which fails the
 * application rather than the draw. GL allows a good deal more, and the rest
 * is what this probe covers:
 *
 *   `line_strip_out`  a shader emitting a line strip, which is the third of
 *                     GL's three output primitives and used to be refused
 *   `lines_in`        a shader taking lines, likewise the second of five
 *   `adjacency_in`    triangles_adjacency in: six vertices per primitive,
 *                     three of them adjacency the shader must be able to read
 *                     and which must not become geometry
 *   `indexed`         the same points-in shader driven by glDrawElements, in
 *                     an order the vertex buffer does not have, so a driver
 *                     that ignored the indices draws the wrong two of three
 *   `instanced`       glDrawArraysInstanced with a per-instance attribute, so
 *                     a driver that ran one instance, or that read the
 *                     attribute per vertex, gets a different picture
 *   `strip_draw`      a triangle-strip *draw* into a `triangles` shader: the
 *                     strip has to be assembled into primitives, and GL's
 *                     vertex order for an odd triangle is (i+1, i, i+2) --
 *                     not the order transform feedback uses, which is why the
 *                     shader below reads gl_in[0] and gl_in[1] apart
 *
 * Every case draws into its own corner of one target and checks a pixel that
 * the other cases cannot explain, so a shape that silently drew nothing is a
 * failure rather than a coincidence.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 64

static GLuint build(const char *gs_src, const char *vs_src) {
    static const char *default_vs =
        "#version 330 core\n"
        "layout(location = 0) in vec2 at;\n"
        "void main() { gl_Position = vec4(at, 0.0, 1.0); }\n";
    static const char *fs_src =
        "#version 330 core\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(0.0, 1.0, 0.0, 1.0); }\n";
    struct { GLenum stage; const char *source; const char *label; } parts[] = {
        {GL_VERTEX_SHADER, vs_src ? vs_src : default_vs, "vertex"},
        {GL_GEOMETRY_SHADER, gs_src, "geometry"},
        {GL_FRAGMENT_SHADER, fs_src, "fragment"},
    };
    GLuint program = glCreateProgram();
    for (unsigned i = 0; i < 3; ++i) {
        GLuint shader = glCreateShader(parts[i].stage);
        glShaderSource(shader, 1, &parts[i].source, NULL);
        glCompileShader(shader);
        GLint ok = 0;
        glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO %s compile: %s\n", parts[i].label, log);
            return 0;
        }
        glAttachShader(program, shader);
    }
    glLinkProgram(program);
    GLint ok = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return program;
}

/* Green anywhere in a small window around the given pixel. A line is one
 * pixel wide and rasterization rules differ by a pixel between drivers, so a
 * line case that sampled a single point would be a coincidence test. */
static int green_near(int x, int y, int radius) {
    const int side = radius * 2 + 1;
    unsigned char *px = (unsigned char *)malloc((size_t)side * side * 4);
    if (!px) return 0;
    glReadPixels(x - radius, y - radius, side, side, GL_RGBA,
                 GL_UNSIGNED_BYTE, px);
    int found = 0;
    for (int i = 0; i < side * side; ++i)
        if (px[i * 4 + 1] > 128 && px[i * 4] < 128) { found = 1; break; }
    free(px);
    return found;
}

static void clear(void) {
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
}

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

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint colour = 0, fbo = 0, vao = 0, buffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 2 * sizeof(float), NULL);

    /* ---- line_strip_out: two points, each expanded into a horizontal line
     * across the middle of its half. A driver with no line-strip output
     * refuses the shader and nothing is drawn. */
    {
        GLuint program = build(
            "#version 330 core\n"
            "layout(points) in;\n"
            "layout(line_strip, max_vertices = 2) out;\n"
            "void main() {\n"
            "  gl_Position = gl_in[0].gl_Position + vec4(-0.3, 0.0, 0.0, 0.0);\n"
            "  EmitVertex();\n"
            "  gl_Position = gl_in[0].gl_Position + vec4( 0.3, 0.0, 0.0, 0.0);\n"
            "  EmitVertex();\n"
            "  EndPrimitive();\n"
            "}\n", NULL);
        const float points[2] = {0.0f, 0.0f};
        glBindBuffer(GL_ARRAY_BUFFER, buffer);
        glBufferData(GL_ARRAY_BUFFER, sizeof(points), points, GL_STREAM_DRAW);
        clear();
        if (program) { glUseProgram(program); glDrawArrays(GL_POINTS, 0, 1); }
        const GLenum error = glGetError();
        /* On the line, and not above it. */
        const int on = green_near(SIDE / 2, SIDE / 2, 1);
        const int off = green_near(SIDE / 2, SIDE / 2 + 12, 1);
        fprintf(stderr, "INFO line_strip_out on=%d off=%d err=0x%04x\n",
                on, off, (unsigned)error);
        result("line_strip_out",
               program && error == GL_NO_ERROR && on && !off);
    }

    /* ---- lines_in: a lines-in shader turning each line into a quad that
     * covers its own half of the target. Two lines, so both halves fill. */
    static const char *lines_gs =
        "#version 330 core\n"
        "layout(lines) in;\n"
        "layout(triangle_strip, max_vertices = 4) out;\n"
        "void main() {\n"
        "  vec4 a = gl_in[0].gl_Position;\n"
        "  vec4 b = gl_in[1].gl_Position;\n"
        "  gl_Position = vec4(a.x, -0.9, 0.0, 1.0); EmitVertex();\n"
        "  gl_Position = vec4(b.x, -0.9, 0.0, 1.0); EmitVertex();\n"
        "  gl_Position = vec4(a.x,  0.9, 0.0, 1.0); EmitVertex();\n"
        "  gl_Position = vec4(b.x,  0.9, 0.0, 1.0); EmitVertex();\n"
        "  EndPrimitive();\n"
        "}\n";
    {
        GLuint program = build(lines_gs, NULL);
        /* One line, from x = -0.8 to x = -0.2: the left quarter fills and the
         * right half stays black, so a shader that read the wrong input
         * vertex covers the wrong span. */
        const float lines[4] = {-0.8f, 0.0f, -0.2f, 0.0f};
        glBindBuffer(GL_ARRAY_BUFFER, buffer);
        glBufferData(GL_ARRAY_BUFFER, sizeof(lines), lines, GL_STREAM_DRAW);
        clear();
        if (program) { glUseProgram(program); glDrawArrays(GL_LINES, 0, 2); }
        const GLenum error = glGetError();
        const int inside = green_near(SIDE / 4, SIDE / 2, 1);
        const int outside = green_near(SIDE * 3 / 4, SIDE / 2, 1);
        fprintf(stderr, "INFO lines_in inside=%d outside=%d err=0x%04x\n",
                inside, outside, (unsigned)error);
        result("lines_in",
               program && error == GL_NO_ERROR && inside && !outside);
    }

    /* ---- adjacency_in: triangles_adjacency gives six vertices, of which
     * gl_in[0], gl_in[2] and gl_in[4] are the triangle and the odd ones are
     * adjacent. The shader emits the triangle from the even slots, so a
     * driver that passed all six through as geometry, or that mixed the two
     * kinds up, draws a different triangle. The adjacent vertices are placed
     * off to the right where they would show. */
    {
        GLuint program = build(
            "#version 330 core\n"
            "layout(triangles_adjacency) in;\n"
            "layout(triangle_strip, max_vertices = 3) out;\n"
            "void main() {\n"
            "  gl_Position = gl_in[0].gl_Position; EmitVertex();\n"
            "  gl_Position = gl_in[2].gl_Position; EmitVertex();\n"
            "  gl_Position = gl_in[4].gl_Position; EmitVertex();\n"
            "  EndPrimitive();\n"
            "}\n", NULL);
        /* Even slots: a triangle over the left half. Odd slots: points far to
         * the right, which must not be drawn. */
        const float adjacent[12] = {
            -0.9f, -0.9f,   0.95f, -0.95f,
            -0.1f, -0.9f,   0.95f,  0.0f,
            -0.9f,  0.9f,   0.95f,  0.95f,
        };
        glBindBuffer(GL_ARRAY_BUFFER, buffer);
        glBufferData(GL_ARRAY_BUFFER, sizeof(adjacent), adjacent,
                     GL_STREAM_DRAW);
        clear();
        if (program) {
            glUseProgram(program);
            glDrawArrays(GL_TRIANGLES_ADJACENCY, 0, 6);
        }
        const GLenum error = glGetError();
        const int inside = green_near(SIDE / 6, SIDE / 2, 1);
        const int outside = green_near(SIDE * 5 / 6, SIDE / 2, 1);
        fprintf(stderr, "INFO adjacency_in inside=%d outside=%d err=0x%04x\n",
                inside, outside, (unsigned)error);
        result("adjacency_in",
               program && error == GL_NO_ERROR && inside && !outside);
    }

    /* ---- indexed: the same lines-in shader, but the two vertices come from
     * the index buffer in an order the array does not have. The array holds
     * the right span third; the indices pick the first and second entries,
     * which span the left quarter. A driver ignoring indices fills the right. */
    {
        GLuint program = build(lines_gs, NULL);
        const float points[6] = {
            -0.8f, 0.0f,   /* 0 */
            -0.2f, 0.0f,   /* 1 */
             0.8f, 0.0f,   /* 2, never indexed */
        };
        const unsigned short indices[2] = {1, 0};
        GLuint element_buffer = 0;
        glGenBuffers(1, &element_buffer);
        glBindBuffer(GL_ARRAY_BUFFER, buffer);
        glBufferData(GL_ARRAY_BUFFER, sizeof(points), points, GL_STREAM_DRAW);
        glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, element_buffer);
        glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(indices), indices,
                     GL_STREAM_DRAW);
        clear();
        if (program) {
            glUseProgram(program);
            glDrawElements(GL_LINES, 2, GL_UNSIGNED_SHORT, NULL);
        }
        const GLenum error = glGetError();
        const int inside = green_near(SIDE / 4, SIDE / 2, 1);
        const int outside = green_near(SIDE * 7 / 8, SIDE / 2, 1);
        fprintf(stderr, "INFO indexed inside=%d outside=%d err=0x%04x\n",
                inside, outside, (unsigned)error);
        result("indexed", program && error == GL_NO_ERROR && inside && !outside);
        glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, 0);
        glDeleteBuffers(1, &element_buffer);
    }

    /* ---- instanced: two instances of a one-point draw, each expanded into a
     * quad, shifted by a *per-instance* attribute. One instance covers the
     * left half and the other the right, so a driver that ran a single
     * instance leaves half black and one that read the attribute per vertex
     * puts both quads in the same place. */
    {
        static const char *instanced_vs =
            "#version 330 core\n"
            "layout(location = 0) in vec2 at;\n"
            "layout(location = 1) in vec2 shift;\n"
            "void main() { gl_Position = vec4(at + shift, 0.0, 1.0); }\n";
        GLuint program = build(
            "#version 330 core\n"
            "layout(points) in;\n"
            "layout(triangle_strip, max_vertices = 4) out;\n"
            "void main() {\n"
            "  vec4 c = gl_in[0].gl_Position;\n"
            "  float h = 0.2;\n"
            "  gl_Position = c + vec4(-h, -h, 0.0, 0.0); EmitVertex();\n"
            "  gl_Position = c + vec4( h, -h, 0.0, 0.0); EmitVertex();\n"
            "  gl_Position = c + vec4(-h,  h, 0.0, 0.0); EmitVertex();\n"
            "  gl_Position = c + vec4( h,  h, 0.0, 0.0); EmitVertex();\n"
            "  EndPrimitive();\n"
            "}\n", instanced_vs);
        const float centre[2] = {0.0f, 0.0f};
        const float shifts[4] = {-0.5f, 0.0f,  0.5f, 0.0f};
        GLuint shift_buffer = 0;
        glGenBuffers(1, &shift_buffer);
        glBindBuffer(GL_ARRAY_BUFFER, buffer);
        glBufferData(GL_ARRAY_BUFFER, sizeof(centre), centre, GL_STREAM_DRAW);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 2 * sizeof(float),
                              NULL);
        glBindBuffer(GL_ARRAY_BUFFER, shift_buffer);
        glBufferData(GL_ARRAY_BUFFER, sizeof(shifts), shifts, GL_STREAM_DRAW);
        glEnableVertexAttribArray(1);
        glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 2 * sizeof(float),
                              NULL);
        glVertexAttribDivisor(1, 1);
        clear();
        if (program) {
            glUseProgram(program);
            glDrawArraysInstanced(GL_POINTS, 0, 1, 2);
        }
        const GLenum error = glGetError();
        /* Clip -0.5 and +0.5 are pixels 16 and 48; the quads are 0.2 wide, so
         * the gap at pixel 32 lies between them. */
        const int left = green_near(16, SIDE / 2, 1);
        const int right = green_near(48, SIDE / 2, 1);
        const int gap = green_near(SIDE / 2, SIDE / 2, 0);
        fprintf(stderr, "INFO instanced left=%d right=%d gap=%d err=0x%04x\n",
                left, right, gap, (unsigned)error);
        result("instanced",
               program && error == GL_NO_ERROR && left && right && !gap);
        glDisableVertexAttribArray(1);
        glVertexAttribDivisor(1, 0);
        glDeleteBuffers(1, &shift_buffer);
        glBindBuffer(GL_ARRAY_BUFFER, buffer);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 2 * sizeof(float),
                              NULL);
    }

    /* ---- strip_draw: a four-vertex triangle strip into a `triangles`
     * shader, which is two primitives. The shader emits nothing for the first
     * and a marker for the second, positioned from gl_in[0] and gl_in[1]
     * separately -- so a driver that assembled the odd triangle in the order
     * transform feedback uses, (i, i+2, i+1) rather than GL's (i+1, i, i+2),
     * puts the marker in the wrong corner. */
    {
        GLuint program = build(
            "#version 330 core\n"
            "layout(triangles) in;\n"
            "layout(triangle_strip, max_vertices = 4) out;\n"
            "void main() {\n"
            "  if (gl_PrimitiveIDIn != 1) return;\n"
            "  /* A small quad *at* gl_in[0], which is the whole point: the\n"
            "     two candidate orderings disagree about which vertex that\n"
            "     is, and agree about everything else. */\n"
            "  vec4 c = gl_in[0].gl_Position;\n"
            "  float h = 0.12;\n"
            "  gl_Position = c + vec4(-h, -h, 0.0, 0.0); EmitVertex();\n"
            "  gl_Position = c + vec4( h, -h, 0.0, 0.0); EmitVertex();\n"
            "  gl_Position = c + vec4(-h,  h, 0.0, 0.0); EmitVertex();\n"
            "  gl_Position = c + vec4( h,  h, 0.0, 0.0); EmitVertex();\n"
            "  EndPrimitive();\n"
            "}\n", NULL);
        /* v0..v3. Triangle 1 -- the odd one -- is GL-ordered (v2, v1, v3),
         * so gl_in[0] is v2, on the left. Assembled the way transform
         * feedback does it the primitive is (v1, v3, v2) and gl_in[0] is v1,
         * on the right. Both orderings hold the same three vertices and the
         * same winding, so where gl_in[0] is is the only thing that tells
         * them apart -- and v1 and v2 sit at opposite sides to make that
         * difference as large as the target allows. */
        const float strip[8] = {
             0.0f,  0.8f,   /* v0 */
             0.7f,  0.0f,   /* v1 */
            -0.7f,  0.0f,   /* v2 */
             0.0f, -0.8f,   /* v3 */
        };
        glBindBuffer(GL_ARRAY_BUFFER, buffer);
        glBufferData(GL_ARRAY_BUFFER, sizeof(strip), strip, GL_STREAM_DRAW);
        clear();
        if (program) {
            glUseProgram(program);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
        const GLenum error = glGetError();
        /* Clip -0.7 is pixel 9, +0.7 is pixel 54. */
        const int left = green_near(9, SIDE / 2, 2);
        const int right = green_near(54, SIDE / 2, 2);
        fprintf(stderr, "INFO strip_draw left=%d right=%d err=0x%04x\n",
                left, right, (unsigned)error);
        result("strip_draw",
               program && error == GL_NO_ERROR && left && !right);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
