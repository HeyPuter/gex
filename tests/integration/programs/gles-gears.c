/* Gears, on OpenGL ES 2.0.
 *
 * The same three interlocking gears everyone has seen since glxgears in 1996,
 * written against EGL and GLES2 like any other application. Nothing here knows
 * about gex: it includes the standard headers, links against libEGL and
 * libGLESv2 by their usual sonames, and would run unchanged on a phone.
 *
 * Under gex those two libraries are marshallers and the drawing happens on the
 * host's GL, so this is the demonstration that a real program with real
 * geometry, real shaders and real per-frame uniforms gets there.
 *
 * Deliberately not glxgears itself: that one is GLX over X11, which under
 * Xwayland renders with swrast on the guest's own processor. It would look the
 * same and prove the opposite.
 *
 * It renders a fixed number of frames and writes the last one out, so a test
 * can check pixels rather than a person having to watch it.
 */
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define WIDTH 320
#define HEIGHT 240
#ifndef M_PI
#define M_PI 3.14159265358979323846
#endif

/* --------------------------------------------------------------- geometry */

/* A gear is a disc with teeth: two faces, an outward rim, and the sides of
 * each tooth. Built as triangles with a normal per vertex, which is what the
 * lighting in the fragment shader needs. */
struct mesh {
    GLfloat *data; /* x, y, z, nx, ny, nz */
    int count;
    int capacity;
};

static void push(struct mesh *m, float x, float y, float z, float nx, float ny,
                 float nz) {
    if (m->count == m->capacity) {
        m->capacity = m->capacity ? m->capacity * 2 : 1024;
        m->data = realloc(m->data, (size_t)m->capacity * 6 * sizeof(GLfloat));
    }
    GLfloat *v = m->data + (size_t)m->count * 6;
    v[0] = x; v[1] = y; v[2] = z; v[3] = nx; v[4] = ny; v[5] = nz;
    m->count++;
}

static void quad(struct mesh *m, const float *a, const float *b, const float *c,
                 const float *d, float nx, float ny, float nz) {
    push(m, a[0], a[1], a[2], nx, ny, nz);
    push(m, b[0], b[1], b[2], nx, ny, nz);
    push(m, c[0], c[1], c[2], nx, ny, nz);
    push(m, a[0], a[1], a[2], nx, ny, nz);
    push(m, c[0], c[1], c[2], nx, ny, nz);
    push(m, d[0], d[1], d[2], nx, ny, nz);
}

static void build_gear(struct mesh *m, float inner, float outer, float width,
                       int teeth, float tooth_depth) {
    float r0 = inner;
    float r1 = outer - tooth_depth / 2.0f;
    float r2 = outer + tooth_depth / 2.0f;
    float da = 2.0f * (float)M_PI / (float)teeth / 4.0f;
    float half = width * 0.5f;

    for (int i = 0; i < teeth; i++) {
        float a = (float)i * 2.0f * (float)M_PI / (float)teeth;
        float c0 = cosf(a), s0 = sinf(a);
        float c1 = cosf(a + da), s1 = sinf(a + da);
        float c2 = cosf(a + 2 * da), s2 = sinf(a + 2 * da);
        float c3 = cosf(a + 3 * da), s3 = sinf(a + 3 * da);
        float c4 = cosf(a + 4 * da), s4 = sinf(a + 4 * da);

        /* Front and back faces, as two quads each: the body and the tooth. */
        for (int side = 0; side < 2; side++) {
            float z = side ? -half : half;
            float nz = side ? -1.0f : 1.0f;
            float p0[3] = {r0 * c0, r0 * s0, z};
            float p1[3] = {r1 * c0, r1 * s0, z};
            float p2[3] = {r1 * c3, r1 * s3, z};
            float p3[3] = {r0 * c4, r0 * s4, z};
            float t0[3] = {r1 * c1, r1 * s1, z};
            float t1[3] = {r2 * c1, r2 * s1, z};
            float t2[3] = {r2 * c2, r2 * s2, z};
            float t3[3] = {r1 * c2, r1 * s2, z};
            if (side) {
                quad(m, p3, p2, p1, p0, 0, 0, nz);
                quad(m, t3, t2, t1, t0, 0, 0, nz);
            } else {
                quad(m, p0, p1, p2, p3, 0, 0, nz);
                quad(m, t0, t1, t2, t3, 0, 0, nz);
            }
        }

        /* The outward surfaces: the rim between teeth and the three faces of
           each tooth, with a normal pointing away from the axis. */
        struct {
            float ra, aa, rb, ab;
        } outward[] = {
            {r1, a, r2, a + da},
            {r2, a + da, r2, a + 2 * da},
            {r2, a + 2 * da, r1, a + 3 * da},
            {r1, a + 3 * da, r1, a + 4 * da},
        };
        for (int k = 0; k < 4; k++) {
            float ca = cosf(outward[k].aa), sa = sinf(outward[k].aa);
            float cb = cosf(outward[k].ab), sb = sinf(outward[k].ab);
            float ax = outward[k].ra * ca, ay = outward[k].ra * sa;
            float bx = outward[k].rb * cb, by = outward[k].rb * sb;
            /* Perpendicular to the edge, in the plane of the gear. */
            float ex = bx - ax, ey = by - ay;
            float len = sqrtf(ex * ex + ey * ey);
            float nx = len > 0 ? ey / len : ca;
            float ny = len > 0 ? -ex / len : sa;
            float q0[3] = {ax, ay, half};
            float q1[3] = {bx, by, half};
            float q2[3] = {bx, by, -half};
            float q3[3] = {ax, ay, -half};
            quad(m, q0, q1, q2, q3, nx, ny, 0);
        }
    }
}

/* ----------------------------------------------------------------- shaders */

static const char *VS =
    "uniform mat4 mvp;\n"
    "uniform mat4 model;\n"
    "uniform vec3 colour;\n"
    "attribute vec3 pos;\n"
    "attribute vec3 normal;\n"
    "varying vec3 v_colour;\n"
    "void main() {\n"
    "  vec3 n = normalize((model * vec4(normal, 0.0)).xyz);\n"
    "  float lit = max(dot(n, normalize(vec3(0.4, 0.5, 0.8))), 0.0);\n"
    "  v_colour = colour * (0.25 + 0.75 * lit);\n"
    "  gl_Position = mvp * vec4(pos, 1.0);\n"
    "}\n";

static const char *FS =
    "precision mediump float;\n"
    "varying vec3 v_colour;\n"
    "void main() { gl_FragColor = vec4(v_colour, 1.0); }\n";

/* ------------------------------------------------------------------ matrix */

static void identity(float *m) {
    memset(m, 0, 16 * sizeof(float));
    m[0] = m[5] = m[10] = m[15] = 1.0f;
}

static void multiply(float *out, const float *a, const float *b) {
    float r[16];
    for (int i = 0; i < 4; i++) {
        for (int j = 0; j < 4; j++) {
            r[i * 4 + j] = a[i * 4 + 0] * b[0 * 4 + j] + a[i * 4 + 1] * b[1 * 4 + j] +
                           a[i * 4 + 2] * b[2 * 4 + j] + a[i * 4 + 3] * b[3 * 4 + j];
        }
    }
    memcpy(out, r, sizeof r);
}

static void translate(float *m, float x, float y, float z) {
    float t[16];
    identity(t);
    t[12] = x; t[13] = y; t[14] = z;
    multiply(m, t, m);
}

static void rotate_z(float *m, float radians) {
    float r[16];
    identity(r);
    r[0] = cosf(radians); r[1] = sinf(radians);
    r[4] = -sinf(radians); r[5] = cosf(radians);
    multiply(m, r, m);
}

static void rotate_x(float *m, float radians) {
    float r[16];
    identity(r);
    r[5] = cosf(radians); r[6] = sinf(radians);
    r[9] = -sinf(radians); r[10] = cosf(radians);
    multiply(m, r, m);
}

static void frustum(float *m, float l, float r, float b, float t, float n, float f) {
    memset(m, 0, 16 * sizeof(float));
    m[0] = 2 * n / (r - l);
    m[5] = 2 * n / (t - b);
    m[8] = (r + l) / (r - l);
    m[9] = (t + b) / (t - b);
    m[10] = -(f + n) / (f - n);
    m[11] = -1.0f;
    m[14] = -2 * f * n / (f - n);
}

/* -------------------------------------------------------------------- main */

static GLuint compile(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(s, sizeof log, NULL, log);
        printf("shader failed: %s\n", log);
        exit(1);
    }
    return s;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    int frames = argc > 1 ? atoi(argv[1]) : 30;
    const char *out_path = argc > 2 ? argv[2] : NULL;

    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(dpy, NULL, NULL)) {
        printf("eglInitialize failed\n");
        return 1;
    }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cfga[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE,
                     EGL_OPENGL_ES2_BIT, EGL_DEPTH_SIZE, 16, EGL_NONE};
    EGLConfig cfg;
    EGLint n = 0;
    eglChooseConfig(dpy, cfga, &cfg, 1, &n);
    EGLint pb[] = {EGL_WIDTH, WIDTH, EGL_HEIGHT, HEIGHT, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, pb);
    EGLint ctxa[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxa);
    eglMakeCurrent(dpy, surf, surf, ctx);
    printf("renderer %s\n", (const char *)glGetString(GL_RENDERER));

    GLuint prog = glCreateProgram();
    glAttachShader(prog, compile(GL_VERTEX_SHADER, VS));
    glAttachShader(prog, compile(GL_FRAGMENT_SHADER, FS));
    glBindAttribLocation(prog, 0, "pos");
    glBindAttribLocation(prog, 1, "normal");
    glLinkProgram(prog);
    GLint linked = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024] = {0};
        glGetProgramInfoLog(prog, sizeof log, NULL, log);
        printf("link failed: %s\n", log);
        return 1;
    }
    glUseProgram(prog);
    GLint u_mvp = glGetUniformLocation(prog, "mvp");
    GLint u_model = glGetUniformLocation(prog, "model");
    GLint u_colour = glGetUniformLocation(prog, "colour");

    struct {
        struct mesh mesh;
        GLuint vbo;
        float x, y, phase;
        float r, g, b;
        float spin;
    } gears[3] = {
        {{0}, 0, -3.0f, -2.0f, 0.0f, 0.8f, 0.1f, 0.0f, 1.0f},
        {{0}, 0, 3.1f, -2.0f, -9.0f, 0.0f, 0.8f, 0.2f, -2.0f},
        {{0}, 0, -3.1f, 4.2f, -25.0f, 0.2f, 0.2f, 1.0f, -2.0f},
    };
    build_gear(&gears[0].mesh, 1.0f, 4.0f, 1.0f, 20, 0.7f);
    build_gear(&gears[1].mesh, 0.5f, 2.0f, 2.0f, 10, 0.7f);
    build_gear(&gears[2].mesh, 1.3f, 2.0f, 0.5f, 10, 0.7f);

    for (int i = 0; i < 3; i++) {
        glGenBuffers(1, &gears[i].vbo);
        glBindBuffer(GL_ARRAY_BUFFER, gears[i].vbo);
        glBufferData(GL_ARRAY_BUFFER,
                     (long)gears[i].mesh.count * 6 * (long)sizeof(GLfloat),
                     gears[i].mesh.data, GL_STATIC_DRAW);
    }
    printf("triangles %d\n",
           (gears[0].mesh.count + gears[1].mesh.count + gears[2].mesh.count) / 3);

    glEnable(GL_DEPTH_TEST);
    glEnable(GL_CULL_FACE);
    glViewport(0, 0, WIDTH, HEIGHT);

    float projection[16];
    float aspect = (float)WIDTH / (float)HEIGHT;
    frustum(projection, -aspect, aspect, -1.0f, 1.0f, 5.0f, 60.0f);

    for (int frame = 0; frame < frames; frame++) {
        float angle = (float)frame * 2.0f;
        glClearColor(0.1f, 0.1f, 0.15f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);

        for (int i = 0; i < 3; i++) {
            /* Built outermost first, because each of these post-multiplies:
               the gear spins about its own axis, is placed beside the others,
               and then the whole arrangement is tilted and pushed away from
               the camera. Writing them the other way round smears the
               geometry across the frame, which is what it did. */
            float model[16];
            identity(model);
            translate(model, 0.0f, 0.0f, -40.0f);
            rotate_x(model, 20.0f * (float)M_PI / 180.0f);
            translate(model, gears[i].x, gears[i].y, 0.0f);
            rotate_z(model, (gears[i].phase + angle * gears[i].spin) * (float)M_PI / 180.0f);

            float mvp[16];
            multiply(mvp, model, projection);
            glUniformMatrix4fv(u_mvp, 1, GL_FALSE, mvp);
            glUniformMatrix4fv(u_model, 1, GL_FALSE, model);
            glUniform3f(u_colour, gears[i].r, gears[i].g, gears[i].b);

            glBindBuffer(GL_ARRAY_BUFFER, gears[i].vbo);
            glEnableVertexAttribArray(0);
            glEnableVertexAttribArray(1);
            glVertexAttribPointer(0, 3, GL_FLOAT, GL_FALSE, 6 * (int)sizeof(GLfloat),
                                  (const void *)0);
            glVertexAttribPointer(1, 3, GL_FLOAT, GL_FALSE, 6 * (int)sizeof(GLfloat),
                                  (const void *)(3 * sizeof(GLfloat)));
            glDrawArrays(GL_TRIANGLES, 0, gears[i].mesh.count);
        }
        eglSwapBuffers(dpy, surf);
    }
    glFinish();
    printf("frames %d\n", frames);
    printf("gl error %d\n", (int)glGetError());

    unsigned char *px = malloc((size_t)WIDTH * HEIGHT * 4);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, px);
    /* How much of the frame is not background: a gear that did not draw shows
       up as zero, and a lighting or depth mistake shows up as a wrong count. */
    long lit = 0;
    for (long i = 0; i < (long)WIDTH * HEIGHT; i++) {
        const unsigned char *p = px + i * 4;
        if (p[0] > 40 || p[1] > 40 || p[2] > 60) {
            lit++;
        }
    }
    printf("covered %ld%%\n", lit * 100 / ((long)WIDTH * HEIGHT));

    if (out_path) {
        FILE *f = fopen(out_path, "wb");
        if (f) {
            fprintf(f, "P6\n%d %d\n255\n", WIDTH, HEIGHT);
            /* GL reads bottom-up; a PPM is top-down. */
            for (int y = HEIGHT - 1; y >= 0; y--) {
                for (int x = 0; x < WIDTH; x++) {
                    fwrite(px + ((long)y * WIDTH + x) * 4, 1, 3, f);
                }
            }
            fclose(f);
            printf("wrote %s\n", out_path);
        }
    }
    free(px);
    return 0;
}
