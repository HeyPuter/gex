/* As drawrate, but each draw changes the state Minecraft's entity renderer
 * changes: a matrix, a colour, and a texture binding. If the draw rate
 * collapses, the cost is st/mesa revalidating state rather than the game's
 * own CPU work. */
#include <GL/glx.h>
#include <GL/gl.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
#define TEXTURES 8
int main(int argc, char **argv) {
    const int per_frame = argc > 1 ? atoi(argv[1]) : 2000;
    const int mode = argc > 2 ? atoi(argv[2]) : 3;   /* bit 1 matrix, 2 colour, 4 texture */
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) return 2;
    int attrs[] = {GLX_RGBA, GLX_DOUBLEBUFFER, GLX_DEPTH_SIZE, 24, None};
    XVisualInfo *vi = glXChooseVisual(dpy, DefaultScreen(dpy), attrs);
    XSetWindowAttributes swa = {0};
    swa.colormap = XCreateColormap(dpy, RootWindow(dpy, vi->screen), vi->visual, AllocNone);
    Window win = XCreateWindow(dpy, RootWindow(dpy, vi->screen), 0,0,640,480,0,
                               vi->depth, InputOutput, vi->visual, CWColormap, &swa);
    XStoreName(dpy, win, "drawrate");
    XMapWindow(dpy, win);
    glXMakeCurrent(dpy, win, glXCreateContext(dpy, vi, NULL, True));
    printf("GL_RENDERER = %s  mode=%d\n", (const char *)glGetString(GL_RENDERER), mode);

    GLuint tex[TEXTURES];
    glGenTextures(TEXTURES, tex);
    unsigned char px[16*16*4];
    for (int t = 0; t < TEXTURES; ++t) {
        for (int i = 0; i < 16*16*4; ++i) px[i] = (unsigned char)((i * (t + 3)) & 0xff);
        glBindTexture(GL_TEXTURE_2D, tex[t]);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 16, 16, 0, GL_RGBA, GL_UNSIGNED_BYTE, px);
    }
    glEnable(GL_TEXTURE_2D);
    static const float tri[9] = {-0.01f,-0.01f,0, 0.01f,-0.01f,0, 0,0.01f,0};
    static const float uv[6]  = {0,0, 1,0, 0.5f,1};
    glEnableClientState(GL_VERTEX_ARRAY);
    glEnableClientState(GL_TEXTURE_COORD_ARRAY);
    glVertexPointer(3, GL_FLOAT, 0, tri);
    glTexCoordPointer(2, GL_FLOAT, 0, uv);

    double frames = 0, draws = 0, start = now(), last = start;
    while (now() - start < 12.0) {
        glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
        for (int i = 0; i < per_frame; ++i) {
            if (mode & 1) { glPushMatrix(); glTranslatef((i%40)*0.02f-0.4f,(i/40)*0.02f-0.4f,0); }
            if (mode & 2) glColor4f((i&7)/7.0f, 0.5f, 1.0f, 1.0f);
            if (mode & 4) glBindTexture(GL_TEXTURE_2D, tex[i % TEXTURES]);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            if (mode & 1) glPopMatrix();
            ++draws;
        }
        glXSwapBuffers(dpy, win); ++frames;
        if (now() - last > 4.0) {
            const double s = now() - last;
            printf("  %.0f draws/s, %.1f frames/s\n", draws/s, frames/s);
            draws = frames = 0; last = now();
        }
    }
    return 0;
}
