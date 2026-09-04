/* Flat shading and glPolygonMode(GL_LINE) together.
 *
 * Each of these two features works on its own in this driver, and the bug is
 * only in their product -- which is why neither the flat-shading probe nor
 * the polygon-mode probe sees it.
 *
 * A flat-shaded primitive takes its varyings from one vertex, its provoking
 * vertex, which for GL's default convention is the primitive's *last*. WGSL's
 * @interpolate(flat) always takes the *first* vertex instead, so this driver
 * serves GL's convention by reordering indices: a triangle (v0,v1,v2) is
 * drawn as (v2,v0,v1), which is the same triangle with the wanted vertex
 * first. That works because a primitive's geometry does not depend on where
 * its vertex list starts.
 *
 * An unfilled polygon breaks the premise. glPolygonMode(GL_LINE) rewrites the
 * polygon into its edges as a LINES draw, and the index reorder then runs on
 * each *line* -- so every edge takes its own second vertex's value and the
 * wireframe comes out with one colour per edge. GL draws all of a polygon's
 * edges in the polygon's single flat colour. The reorder cannot fix it: a
 * triangle's edge (v0,v1) does not contain v2 at all, so no permutation of
 * that edge's two indices can put v2's value on it.
 *
 *   `tri_edges_agree`    the three edges of a flat unfilled triangle are all
 *                        one colour, and it is the third vertex's
 *   `quad_edges_agree`   the same for a quad, whose opposite edge misses the
 *                        provoking vertex by two rather than one
 *   `shader_flat_edges`  the same with a `flat out` varying from a real
 *                        shader rather than fixed-function glColor, which is
 *                        what says whether this is a legacy-only defect
 *
 * Each case samples the middle of every edge and compares the edges to each
 * other *and* to the expected vertex, so a driver that painted them all one
 * wrong colour fails as surely as one that painted them differently.
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
static unsigned char pixels[SIDE * SIDE * 4];

static void read_target(void) {
    memset(pixels, 0, sizeof(pixels));
    glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
}

/* The colour of the edge that passes near (cx, cy).
 *
 * A one-pixel line at an exact coordinate is a rasterisation tie, and which
 * side of it the line lands on is not what this probe is about -- so rather
 * than sampling a computed pixel this searches a small neighbourhood and
 * takes the brightest lit one. Returns 0 when the edge is missing entirely,
 * which keeps a driver that drew nothing from passing an "all edges agree"
 * check.
 */
static int edge_colour(int cx, int cy, unsigned char out[3]) {
    int best = -1;
    out[0] = out[1] = out[2] = 0;
    for (int dy = -3; dy <= 3; ++dy) {
        for (int dx = -3; dx <= 3; ++dx) {
            const int x = cx + dx, y = cy + dy;
            if (x < 0 || y < 0 || x >= SIDE || y >= SIDE) continue;
            const unsigned char *p = &pixels[((unsigned)y * SIDE + x) * 4];
            const int sum = p[0] + p[1] + p[2];
            if (sum > best) { best = sum; out[0] = p[0]; out[1] = p[1];
                              out[2] = p[2]; }
        }
    }
    return best > 40;
}

/* Lit pixels strictly inside the quad, away from its four real edges.
 *
 * Any spurious edge -- the diagonal a quad decomposition introduces -- has to
 * cross this band, and no real edge of a quad spanning device 9.6..54.4 comes
 * near it. Counting the interior rather than sampling a computed diagonal is
 * what makes this independent of *which* diagonal a split happens to choose:
 * an earlier version of this probe sampled x+y=64 while the diagonal ran
 * along x+y=63 and reported no diagonal for a driver that was drawing one.
 */
static int lit_inside_quad(void) {
    int n = 0;
    for (unsigned y = 15; y <= 49; ++y)
        for (unsigned x = 15; x <= 49; ++x) {
            const unsigned char *p = &pixels[((unsigned)y * SIDE + x) * 4];
            if (p[0] > 40 || p[1] > 40 || p[2] > 40) ++n;
        }
    return n;
}

static int same_colour(const unsigned char a[3], const unsigned char b[3]) {
    for (unsigned c = 0; c < 3; ++c) {
        const int d = (int)a[c] - (int)b[c];
        if (d > 40 || d < -40) return 0;
    }
    return 1;
}

static GLuint compile(GLenum stage, const char *source) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        return 0;
    }
    return shader;
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    /* A compatibility context: two of the three cases are fixed-function, and
     * glShadeModel and glPolygonMode are what this probe is about. */
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint colour = 0, fbo = 0;
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
    glPolygonMode(GL_FRONT_AND_BACK, GL_LINE);
    glShadeModel(GL_FLAT);
    glDisable(GL_CULL_FACE);

    /* 1. A flat unfilled triangle. Provoking vertex is the last, so every
     * edge is blue -- including the bottom edge, which does not touch it. */
    {
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glBegin(GL_TRIANGLES);
        glColor3f(1.0f, 0.0f, 0.0f); glVertex2f(-0.8f, -0.8f);
        glColor3f(0.0f, 1.0f, 0.0f); glVertex2f( 0.8f, -0.8f);
        glColor3f(0.0f, 0.0f, 1.0f); glVertex2f( 0.0f,  0.8f);
        glEnd();
        const GLenum error = glGetError();
        read_target();
        /* Clip (0,-0.8) is device (32, 6.4); the two slanted edges are
         * sampled halfway up, at clip (+-0.4, 0) -> (44.8, 32) and
         * (19.2, 32). */
        unsigned char bottom[3], right[3], left[3];
        const int got_bottom = edge_colour(32, 6, bottom);
        const int got_right = edge_colour(45, 32, right);
        const int got_left = edge_colour(19, 32, left);
        fprintf(stderr, "INFO tri bottom=%u,%u,%u right=%u,%u,%u "
                "left=%u,%u,%u err=0x%04x\n",
                bottom[0], bottom[1], bottom[2], right[0], right[1], right[2],
                left[0], left[1], left[2], (unsigned)error);
        static const unsigned char want[3] = {0, 0, 255};
        result("tri_edges_agree",
               error == GL_NO_ERROR && got_bottom && got_right && got_left &&
               same_colour(bottom, want) && same_colour(right, want) &&
               same_colour(left, want));
    }

    /* 2. A flat unfilled quad. Its provoking vertex is the fourth, and the
     * bottom edge (v0,v1) is two vertices away from it -- so a driver that
     * happened to shift each edge's value by one still fails here. */
    {
        glClear(GL_COLOR_BUFFER_BIT);
        glBegin(GL_QUADS);
        glColor3f(1.0f, 0.0f, 0.0f); glVertex2f(-0.7f, -0.7f);
        glColor3f(0.0f, 1.0f, 0.0f); glVertex2f( 0.7f, -0.7f);
        glColor3f(1.0f, 1.0f, 0.0f); glVertex2f( 0.7f,  0.7f);
        glColor3f(0.0f, 0.0f, 1.0f); glVertex2f(-0.7f,  0.7f);
        glEnd();
        const GLenum error = glGetError();
        read_target();
        /* The quad spans device 9.6 .. 54.4 on both axes. */
        unsigned char bottom[3], top[3], right[3], left[3];
        const int ok_b = edge_colour(32, 10, bottom);
        const int ok_t = edge_colour(32, 54, top);
        const int ok_r = edge_colour(54, 32, right);
        const int ok_l = edge_colour(10, 32, left);
        fprintf(stderr, "INFO quad bottom=%u,%u,%u top=%u,%u,%u "
                "right=%u,%u,%u left=%u,%u,%u err=0x%04x\n",
                bottom[0], bottom[1], bottom[2], top[0], top[1], top[2],
                right[0], right[1], right[2], left[0], left[1], left[2],
                (unsigned)error);
        static const unsigned char want[3] = {0, 0, 255};
        result("quad_edges_agree",
               error == GL_NO_ERROR && ok_b && ok_t && ok_r && ok_l &&
               same_colour(bottom, want) && same_colour(top, want) &&
               same_colour(right, want) && same_colour(left, want));
    }
    /* 2b. And no diagonal across it.
     *
     * GL draws a quad's four edges, not the edges of the triangles a driver
     * happens to split it into. This driver does not advertise MESA_PRIM_QUADS
     * so Mesa splits every quad, and it marks the introduced diagonal with a
     * *edge flag* of zero -- that is what edge flags are for. A driver that
     * ignores them draws a fifth edge straight across the middle, which no
     * amount of getting the colours right would hide.
     *
     * Sampled along the anti-diagonal, between the two corners Mesa's split
     * joins, and away from the real edges so only a spurious line can light
     * these pixels.
     */
    {
        read_target();
        const int inside = lit_inside_quad();
        fprintf(stderr, "INFO quad_no_diagonal inside=%d want=0\n", inside);
        result("quad_no_diagonal", inside == 0);
    }


    /* 2e. And the same, smooth-shaded.
     *
     * This is a separate case from the flat one above and not a redundant
     * copy of it: with no flat varying, neither the provoking-vertex rewrite
     * nor the flat-unfilled replay runs, so a smooth unfilled quad reaches
     * the plain edge-list path and nothing else. The pixel comparison found
     * the driver drawing a diagonal there -- `quad-unfilled` differs from
     * llvmpipe by 0.0446 while llvmpipe and softpipe are *pixel-identical*,
     * and the differing pixels are exactly 224, one per row and one per
     * column, which is a line from corner to corner.
     */
    {
        glShadeModel(GL_SMOOTH);
        glClear(GL_COLOR_BUFFER_BIT);
        glBegin(GL_QUADS);
        glColor3f(1.0f, 0.0f, 0.0f); glVertex2f(-0.7f, -0.7f);
        glColor3f(0.0f, 1.0f, 0.0f); glVertex2f( 0.7f, -0.7f);
        glColor3f(1.0f, 1.0f, 0.0f); glVertex2f( 0.7f,  0.7f);
        glColor3f(0.0f, 0.0f, 1.0f); glVertex2f(-0.7f,  0.7f);
        glEnd();
        const GLenum error = glGetError();
        glShadeModel(GL_FLAT);
        read_target();
        const int inside = lit_inside_quad();
        int total = 0;
        for (unsigned i = 0; i < SIDE * SIDE; ++i)
            if (pixels[i * 4] > 40 || pixels[i * 4 + 1] > 40 ||
                pixels[i * 4 + 2] > 40) ++total;
        fprintf(stderr, "INFO smooth_quad_no_diagonal inside=%d total_lit=%d "
                "want inside=0 err=0x%04x\n", inside, total,
                (unsigned)error);
        result("smooth_quad_no_diagonal",
               error == GL_NO_ERROR && inside == 0);
    }

    /* 2c. The same triangle again with back-face culling on.
     *
     * This is a different route through the driver, not a different feature.
     * WebGPU's cullMode acts only on triangle topology, and by the time an
     * unfilled polygon is drawn it is lines -- so the driver culls on the CPU
     * from a capture of the vertex shader's positions, and that filter emits
     * the final edge list *itself* rather than going through the unfilled
     * translator. Edge flags take the same route, for the same reason.
     *
     * So the provoking vertex has to be carried down both routes, and a fix
     * to one of them leaves the other drawing a wireframe with one colour per
     * edge. The triangle here is wound front-facing, so culling the back
     * changes nothing about what is drawn -- only which code draws it.
     */
    {
        glEnable(GL_CULL_FACE);
        glCullFace(GL_BACK);
        glFrontFace(GL_CCW);
        glClear(GL_COLOR_BUFFER_BIT);
        glBegin(GL_TRIANGLES);
        glColor3f(1.0f, 0.0f, 0.0f); glVertex2f(-0.8f, -0.8f);
        glColor3f(0.0f, 1.0f, 0.0f); glVertex2f( 0.8f, -0.8f);
        glColor3f(0.0f, 0.0f, 1.0f); glVertex2f( 0.0f,  0.8f);
        glEnd();
        const GLenum error = glGetError();
        glDisable(GL_CULL_FACE);
        read_target();
        unsigned char bottom[3], right[3], left[3];
        const int got_bottom = edge_colour(32, 6, bottom);
        const int got_right = edge_colour(45, 32, right);
        const int got_left = edge_colour(19, 32, left);
        fprintf(stderr, "INFO culled bottom=%u,%u,%u right=%u,%u,%u "
                "left=%u,%u,%u err=0x%04x\n",
                bottom[0], bottom[1], bottom[2], right[0], right[1], right[2],
                left[0], left[1], left[2], (unsigned)error);
        static const unsigned char want[3] = {0, 0, 255};
        result("culled_edges_agree",
               error == GL_NO_ERROR && got_bottom && got_right && got_left &&
               same_colour(bottom, want) && same_colour(right, want) &&
               same_colour(left, want));
    }

    /* 2d. A triangle fan, whose provoking vertex the driver has to work out
     * from a different formula: a fan's primitive p is (v0, v(p+1), v(p+2)),
     * so its last vertex is not at a fixed stride from the primitive number
     * the way GL_TRIANGLES' is. One triangle is enough to tell a wrong
     * formula from a right one, and keeps the expected colour unambiguous.
     */
    {
        glClear(GL_COLOR_BUFFER_BIT);
        glBegin(GL_TRIANGLE_FAN);
        glColor3f(1.0f, 0.0f, 0.0f); glVertex2f(-0.8f, -0.8f);
        glColor3f(0.0f, 1.0f, 0.0f); glVertex2f( 0.8f, -0.8f);
        glColor3f(0.0f, 0.0f, 1.0f); glVertex2f( 0.0f,  0.8f);
        glEnd();
        const GLenum error = glGetError();
        read_target();
        unsigned char bottom[3], right[3], left[3];
        const int got_bottom = edge_colour(32, 6, bottom);
        const int got_right = edge_colour(45, 32, right);
        const int got_left = edge_colour(19, 32, left);
        fprintf(stderr, "INFO fan bottom=%u,%u,%u right=%u,%u,%u "
                "left=%u,%u,%u err=0x%04x\n",
                bottom[0], bottom[1], bottom[2], right[0], right[1], right[2],
                left[0], left[1], left[2], (unsigned)error);
        static const unsigned char want[3] = {0, 0, 255};
        result("fan_edges_agree",
               error == GL_NO_ERROR && got_bottom && got_right && got_left &&
               same_colour(bottom, want) && same_colour(right, want) &&
               same_colour(left, want));
    }

    /* 2f. A flat unfilled GL_POLYGON, whose provoking vertex is the one
     * exception in GL's flatshading table.
     *
     * Every other topology takes its primitive's *last* vertex. A single
     * polygon takes vertex **one**, its first, and glProvokingVertex does not
     * change that -- the table names vertex 1 outright. Applying the
     * last-vertex rule painted poly-flat-unfilled-clip cyan where both
     * references draw red.
     *
     * Laid out as the same square as the quad cases so the four edge
     * midpoints are the same pixels, with the expected colour on v0 and three
     * different colours behind it, so taking any other vertex fails.
     */
    {
        glClear(GL_COLOR_BUFFER_BIT);
        glBegin(GL_POLYGON);
        glColor3f(0.0f, 0.0f, 1.0f); glVertex2f(-0.7f, -0.7f);
        glColor3f(1.0f, 0.0f, 0.0f); glVertex2f( 0.7f, -0.7f);
        glColor3f(0.0f, 1.0f, 0.0f); glVertex2f( 0.7f,  0.7f);
        glColor3f(1.0f, 1.0f, 0.0f); glVertex2f(-0.7f,  0.7f);
        glEnd();
        const GLenum error = glGetError();
        read_target();
        unsigned char bottom[3], top[3], right[3], left[3];
        const int ok_b = edge_colour(32, 10, bottom);
        const int ok_t = edge_colour(32, 54, top);
        const int ok_r = edge_colour(54, 32, right);
        const int ok_l = edge_colour(10, 32, left);
        const int inside = lit_inside_quad();
        fprintf(stderr, "INFO polygon bottom=%u,%u,%u top=%u,%u,%u "
                "right=%u,%u,%u left=%u,%u,%u inside=%d err=0x%04x\n",
                bottom[0], bottom[1], bottom[2], top[0], top[1], top[2],
                right[0], right[1], right[2], left[0], left[1], left[2],
                inside, (unsigned)error);
        static const unsigned char want[3] = {0, 0, 255};
        result("polygon_edges_agree",
               error == GL_NO_ERROR && ok_b && ok_t && ok_r && ok_l &&
               inside == 0 &&
               same_colour(bottom, want) && same_colour(top, want) &&
               same_colour(right, want) && same_colour(left, want));
    }

    /* 2g. A user clip plane cutting an unfilled quad in half.
     *
     * GL clips the polygon and *then* applies polygon mode, so the clipped
     * polygon's outline includes the edge clipping introduced along the
     * plane. This driver applies polygon mode first and lets the GPU clip the
     * resulting lines, which gets every original edge right and draws no
     * boundary edge at all -- three edges where GL draws four.
     *
     * Both references agree exactly on this (llvmpipe and radeonsi
     * pixel-identical, 134 lit pixels against 90), and the unclipped control
     * matches perfectly, so the boundary edge is the whole of the
     * difference. It is also the whole of the residual on the comparison's
     * three surviving `-clip` rows.
     *
     * The assertion counts lit pixels in a vertical band at the plane, well
     * away from the quad's own left and right edges, so only the introduced
     * edge can light them.
     */
    {
        const double plane[4] = {-1.0, 0.0, 0.0, 0.0};   /* keep x <= 0 */
        glClipPlane(GL_CLIP_PLANE0, plane);
        glEnable(GL_CLIP_PLANE0);
        glClear(GL_COLOR_BUFFER_BIT);
        glColor3f(1.0f, 0.0f, 0.0f);
        glBegin(GL_QUADS);
        glVertex2f(-0.7f, -0.7f); glVertex2f( 0.7f, -0.7f);
        glVertex2f( 0.7f,  0.7f); glVertex2f(-0.7f,  0.7f);
        glEnd();
        const GLenum error = glGetError();
        glDisable(GL_CLIP_PLANE0);
        read_target();
        /* Clip x = 0 is device x = 32. The quad's own edges sit at 9.6 and
         * 54.4, so a band of 28..35 can only be the introduced edge. Rows
         * 15..49 exclude the top and bottom edges. */
        int on_boundary = 0;
        for (unsigned y = 15; y <= 49; ++y)
            for (unsigned x = 28; x <= 35; ++x) {
                const unsigned char *p =
                    &pixels[((unsigned)y * SIDE + x) * 4];
                if (p[0] > 40 || p[1] > 40 || p[2] > 40) ++on_boundary;
            }
        fprintf(stderr, "INFO clip_boundary_edge lit_at_plane=%d want>=30 "
                "err=0x%04x\n", on_boundary, (unsigned)error);
        result("clip_boundary_edge",
               error == GL_NO_ERROR && on_boundary >= 30);
    }

    /* 2h. A *wide* unfilled quad, which is two of this driver's rewrites
     * meeting.
     *
     * glLineWidth above one is served by drawing each segment as a pair of
     * triangles, and polygon mode was still GL_LINE when that draw arrived
     * back at the unfilled rewrite -- so each quad was turned into its own
     * wireframe. The result was a one-pixel outline plus a sloped band per
     * edge, which is each quad's outline and its diagonal: 520 lit pixels
     * against llvmpipe's 696, and interior pixels where there should be
     * none.
     *
     * Polygon mode has already been applied by the time that draw exists, so
     * the fix is for it not to apply twice. Checked two ways: the top edge
     * has to be several pixels thick, and the middle of the quad has to stay
     * empty -- a wireframed quad fails the second even where it passes the
     * first.
     */
    {
        glLineWidth(4.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glColor3f(1.0f, 0.0f, 0.0f);
        glBegin(GL_QUADS);
        glVertex2f(-0.7f, -0.7f); glVertex2f( 0.7f, -0.7f);
        glVertex2f( 0.7f,  0.7f); glVertex2f(-0.7f,  0.7f);
        glEnd();
        const GLenum error = glGetError();
        glLineWidth(1.0f);
        read_target();
        /* A column through the middle of the top edge, which sits at device
         * y = 54.4; a band of 48..60 catches it at any plausible width. */
        int thickness = 0;
        for (unsigned y = 48; y <= 60; ++y) {
            const unsigned char *p = &pixels[((unsigned)y * SIDE + 32) * 4];
            if (p[0] > 40 || p[1] > 40 || p[2] > 40) ++thickness;
        }
        const int inside = lit_inside_quad();
        fprintf(stderr, "INFO wide_unfilled thickness=%d want>=3 inside=%d "
                "want=0 err=0x%04x\n", thickness, inside, (unsigned)error);
        result("wide_unfilled",
               error == GL_NO_ERROR && thickness >= 3 && inside == 0);
    }

    /* 3. The same triangle with a `flat out` varying from a real shader.
     * Fixed-function flat shading is lowered by st/mesa into exactly such a
     * varying, so this should behave identically -- and if it does not, the
     * defect reaches modern shader code and is not a legacy-only concern. */
    {
        static const char *vs_src =
            "#version 330 core\n"
            "layout(location = 0) in vec2 position;\n"
            "layout(location = 1) in vec3 tint;\n"
            "flat out vec3 vs_tint;\n"
            "void main() {\n"
            "  gl_Position = vec4(position, 0.0, 1.0);\n"
            "  vs_tint = tint;\n"
            "}\n";
        static const char *fs_src =
            "#version 330 core\n"
            "flat in vec3 vs_tint;\n"
            "out vec4 colour;\n"
            "void main() { colour = vec4(vs_tint, 1.0); }\n";
        GLuint vs = compile(GL_VERTEX_SHADER, vs_src);
        GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src);
        GLuint program = 0;
        if (vs && fs) {
            program = glCreateProgram();
            glAttachShader(program, vs);
            glAttachShader(program, fs);
            glLinkProgram(program);
            GLint ok = 0;
            glGetProgramiv(program, GL_LINK_STATUS, &ok);
            if (!ok) {
                char log[900] = {0};
                glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
                fprintf(stderr, "INFO link: %s\n", log);
                program = 0;
            }
        }
        if (!program) {
            fprintf(stderr, "INFO the shader did not build\n");
            result("shader_flat_edges", 0);
        } else {
            const float data[15] = {
                -0.8f, -0.8f,  1.0f, 0.0f, 0.0f,
                 0.8f, -0.8f,  0.0f, 1.0f, 0.0f,
                 0.0f,  0.8f,  0.0f, 0.0f, 1.0f,
            };
            GLuint vao = 0, buffer = 0;
            glGenVertexArrays(1, &vao);
            glBindVertexArray(vao);
            glGenBuffers(1, &buffer);
            glBindBuffer(GL_ARRAY_BUFFER, buffer);
            glBufferData(GL_ARRAY_BUFFER, sizeof(data), data, GL_STATIC_DRAW);
            glEnableVertexAttribArray(0);
            glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE,
                                  5 * sizeof(float), NULL);
            glEnableVertexAttribArray(1);
            glVertexAttribPointer(1, 3, GL_FLOAT, GL_FALSE, 5 * sizeof(float),
                                  (const void *)(2 * sizeof(float)));
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(program);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            const GLenum error = glGetError();
            read_target();
            unsigned char bottom[3], right[3], left[3];
            const int got_bottom = edge_colour(32, 6, bottom);
            const int got_right = edge_colour(45, 32, right);
            const int got_left = edge_colour(19, 32, left);
            fprintf(stderr, "INFO shader bottom=%u,%u,%u right=%u,%u,%u "
                    "left=%u,%u,%u err=0x%04x\n",
                    bottom[0], bottom[1], bottom[2],
                    right[0], right[1], right[2],
                    left[0], left[1], left[2], (unsigned)error);
            static const unsigned char want[3] = {0, 0, 255};
            result("shader_flat_edges",
                   error == GL_NO_ERROR && got_bottom && got_right &&
                   got_left && same_colour(bottom, want) &&
                   same_colour(right, want) && same_colour(left, want));
            glUseProgram(0);
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
