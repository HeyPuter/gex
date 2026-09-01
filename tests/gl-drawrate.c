/* How many draw calls a second does the whole stack manage?
 *
 * Deliberately trivial per draw: three vertices, one tiny triangle, no state
 * change between draws beyond a uniform. What it measures is the fixed cost
 * of a draw call through st/mesa and the driver, which is what a scene of
 * many small objects -- a lobby full of entities -- is made of. */
#include <GL/glx.h>
#include <GL/gl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
int main(int argc, char **argv) {
    const int per_frame = argc > 1 ? atoi(argv[1]) : 2000;
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) { fprintf(stderr, "no display\n"); return 2; }
    int attrs[] = {GLX_RGBA, GLX_DOUBLEBUFFER, GLX_DEPTH_SIZE, 24, None};
    XVisualInfo *vi = glXChooseVisual(dpy, DefaultScreen(dpy), attrs);
    if (!vi) { fprintf(stderr, "no visual\n"); return 2; }
    XSetWindowAttributes swa = {0};
    swa.colormap = XCreateColormap(dpy, RootWindow(dpy, vi->screen), vi->visual, AllocNone);
    Window win = XCreateWindow(dpy, RootWindow(dpy, vi->screen), 0, 0, 640, 480, 0,
                               vi->depth, InputOutput, vi->visual, CWColormap, &swa);
    XStoreName(dpy, win, "drawrate");
    XMapWindow(dpy, win);
    GLXContext cx = glXCreateContext(dpy, vi, NULL, True);
    glXMakeCurrent(dpy, win, cx);
    printf("GL_RENDERER = %s\n", (const char *)glGetString(GL_RENDERER));

    /* One small buffer, drawn over and over from a client array -- the shape
     * Minecraft's entity renderer has. */
    static const float tri[9] = {-0.01f,-0.01f,0, 0.01f,-0.01f,0, 0,0.01f,0};
    glEnableClientState(GL_VERTEX_ARRAY);
    glVertexPointer(3, GL_FLOAT, 0, tri);

    double frames = 0, draws = 0, start = now(), last = start;
    while (now() - start < 12.0) {
        glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
        for (int i = 0; i < per_frame; ++i) {
            glPushMatrix();
            glTranslatef((i % 40) * 0.02f - 0.4f, (i / 40) * 0.02f - 0.4f, 0);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            glPopMatrix();
            ++draws;
        }
        glXSwapBuffers(dpy, win);
        ++frames;
        if (now() - last > 4.0) {
            const double s = now() - last;
            printf("  %.0f draws/s, %.1f frames/s\n", draws / s, frames / s);
            draws = frames = 0; last = now();
        }
    }
    return 0;
}
