/* An ordinary OpenGL ES 2.0 program.
 *
 * Nothing here knows about gex. It includes the standard headers, calls the
 * standard functions in the standard order, and links against libEGL and
 * libGLESv2 by their usual names. It is the test that the guest's GL driver
 * can be swapped for one that runs on the host without the application
 * being able to tell.
 *
 * It draws a green triangle over the lower-left half of a blue field and
 * reads the pixels back, which is checkable without a screen.
 */
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <stdio.h>
#include <stdlib.h>

#define W 64
#define H 64

static const char *VS =
    "attribute vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

static const char *FS =
    "precision mediump float;\n"
    "void main() { gl_FragColor = vec4(0.0, 1.0, 0.0, 1.0); }\n";

static GLuint compile(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[512] = {0};
        glGetShaderInfoLog(s, sizeof log, NULL, log);
        printf("shader failed: %s\n", log);
        exit(1);
    }
    return s;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    EGLint major = 0, minor = 0;
    if (!eglInitialize(dpy, &major, &minor)) {
        printf("eglInitialize failed\n");
        return 1;
    }
    printf("EGL %d.%d\n", major, minor);
    eglBindAPI(EGL_OPENGL_ES_API);

    EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                          EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
                          EGL_NONE};
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n) || n < 1) {
        printf("no config\n");
        return 1;
    }
    EGLint pb[] = {EGL_WIDTH, W, EGL_HEIGHT, H, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, pb);
    EGLint ctx_attrs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (!eglMakeCurrent(dpy, surf, surf, ctx)) {
        printf("makeCurrent failed\n");
        return 1;
    }
    printf("GL_VERSION %s\n", (const char *)glGetString(GL_VERSION));

    GLuint prog = glCreateProgram();
    glAttachShader(prog, compile(GL_VERTEX_SHADER, VS));
    glAttachShader(prog, compile(GL_FRAGMENT_SHADER, FS));
    glBindAttribLocation(prog, 0, "pos");
    glLinkProgram(prog);
    GLint linked = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[512] = {0};
        glGetProgramInfoLog(prog, sizeof log, NULL, log);
        printf("link failed: %s\n", log);
        return 1;
    }
    glUseProgram(prog);

    static const GLfloat verts[] = {-1.0f, -1.0f, 0.0f, -1.0f, -1.0f, 0.0f};
    GLuint vbo = 0;
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof verts, verts, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, (const void *)0);

    glViewport(0, 0, W, H);
    glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glFinish();

    printf("gl error %d\n", (int)glGetError());

    unsigned char *px = malloc(W * H * 4);
    glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
#define AT(x, y) (px + ((y) * W + (x)) * 4)
    printf("inside  %d %d %d\n", AT(4, 4)[0], AT(4, 4)[1], AT(4, 4)[2]);
    printf("outside %d %d %d\n", AT(60, 60)[0], AT(60, 60)[1], AT(60, 60)[2]);
    free(px);
    eglDestroyContext(dpy, ctx);
    eglTerminate(dpy);
    return 0;
}
