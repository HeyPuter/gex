/* What Half-Life does every frame, alone: an SDL2 window with SDL's default
 * GL context, the scene drawn (fixed function) into a texture-rectangle
 * framebuffer object, that blitted to the window's framebuffer, swap.
 *
 *   sdl-fbo-blit [MODE] [FRAMES] [WINDOW]
 *
 * MODE: clear  -- glClear of the window only (the floor)
 *       fbo    -- clear + quad into the FBO, blit to 0 (Half-Life's path)
 *       info   -- print GL_VERSION, GL_RENDERER and every extension, exit
 * WINDOW: window (default), fullscreen (SDL_WINDOW_FULLSCREEN, as Half-Life
 *         opens by default) or desktop (SDL_WINDOW_FULLSCREEN_DESKTOP); a
 *         fullscreen window draws at its drawable size, read every frame.
 * Prints "frame N" every 60 frames and reads back the window's centre pixel
 * after the first frame and the 121st ("centre r g b"), the second once a
 * fullscreen window has its size.
 */
#include <SDL2/SDL.h>
#include <stdio.h>
#include <string.h>

typedef unsigned GLenum, GLuint, GLbitfield;
typedef int GLint, GLsizei;
typedef float GLfloat;
#define GL_COLOR_BUFFER_BIT 0x4000
#define GL_TEXTURE_RECTANGLE 0x84F5
#define GL_RGBA 0x1908
#define GL_RGBA8 0x8058
#define GL_UNSIGNED_BYTE 0x1401
#define GL_FRAMEBUFFER 0x8D40
#define GL_READ_FRAMEBUFFER 0x8CA8
#define GL_DRAW_FRAMEBUFFER 0x8CA9
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_FRAMEBUFFER_COMPLETE 0x8CD5
#define GL_LINEAR 0x2601
#define GL_QUADS 0x0007

#define FNS(X) \
  X(void, glClearColor, (GLfloat, GLfloat, GLfloat, GLfloat)) \
  X(void, glClear, (GLbitfield)) \
  X(void, glViewport, (GLint, GLint, GLsizei, GLsizei)) \
  X(void, glGenTextures, (GLsizei, GLuint *)) \
  X(void, glBindTexture, (GLenum, GLuint)) \
  X(void, glTexImage2D, (GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, const void *)) \
  X(void, glGenFramebuffers, (GLsizei, GLuint *)) \
  X(void, glBindFramebuffer, (GLenum, GLuint)) \
  X(void, glFramebufferTexture2D, (GLenum, GLenum, GLenum, GLuint, GLint)) \
  X(GLenum, glCheckFramebufferStatus, (GLenum)) \
  X(void, glBlitFramebuffer, (GLint, GLint, GLint, GLint, GLint, GLint, GLint, GLint, GLbitfield, GLenum)) \
  X(void, glBegin, (GLenum)) \
  X(void, glEnd, (void)) \
  X(void, glColor3f, (GLfloat, GLfloat, GLfloat)) \
  X(void, glVertex2f, (GLfloat, GLfloat)) \
  X(void, glReadPixels, (GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, void *)) \
  X(GLenum, glGetError, (void)) \
  X(const unsigned char *, glGetString, (GLenum)) \
  X(const unsigned char *, glGetStringi, (GLenum, GLuint)) \
  X(void, glGetIntegerv, (GLenum, GLint *)) \
  X(void, glGetFramebufferAttachmentParameteriv, (GLenum, GLenum, GLenum, GLint *))
#define DECL(r, n, a) static r (*p_##n) a;
FNS(DECL)

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    const char *mode = argc > 1 ? argv[1] : "fbo";
    int frames = argc > 2 ? atoi(argv[2]) : 1 << 30;
    const char *how = argc > 3 ? argv[3] : "window";
    Uint32 fs = !strcmp(how, "fullscreen") ? SDL_WINDOW_FULLSCREEN : !strcmp(how, "desktop") ? SDL_WINDOW_FULLSCREEN_DESKTOP : 0;
    int w = 800, h = 600;
    if (SDL_Init(SDL_INIT_VIDEO) != 0) { printf("SDL_Init: %s\n", SDL_GetError()); return 1; }
    SDL_Window *win = SDL_CreateWindow("sdl-fbo-blit", 0, 0, w, h, SDL_WINDOW_OPENGL | fs);
    if (!win) { printf("window: %s\n", SDL_GetError()); return 1; }
    SDL_GLContext ctx = SDL_GL_CreateContext(win);
    if (!ctx) { printf("context: %s\n", SDL_GetError()); return 1; }
#define LOAD(r, n, a) p_##n = (r (*) a)SDL_GL_GetProcAddress(#n); \
    if (!p_##n) { printf("no %s\n", #n); return 1; }
    FNS(LOAD)
    if (!strcmp(mode, "info")) {
        GLint n = 0, v = -1;
        printf("GL_VERSION %s\nGL_RENDERER %s\n", p_glGetString(0x1F02), p_glGetString(0x1F01));
        /* What an application asks about the window's framebuffer. */
        p_glGetIntegerv(0x8CA6 /* DRAW_FRAMEBUFFER_BINDING */, &v);
        printf("DRAW_FRAMEBUFFER_BINDING %d error 0x%x\n", v, p_glGetError());
        v = -1;
        p_glGetIntegerv(0x8CAA /* READ_FRAMEBUFFER_BINDING */, &v);
        printf("READ_FRAMEBUFFER_BINDING %d error 0x%x\n", v, p_glGetError());
        static const GLenum att[] = {0x0402 /* BACK_LEFT */, 0x1801 /* DEPTH */, 0x1802 /* STENCIL */};
        static const GLenum pn[] = {0x8CD0 /* OBJECT_TYPE */, 0x8212, 0x8216 /* DEPTH_SIZE */, 0x8217};
        for (int a = 0; a < 3; a++)
            for (int q = 0; q < 4; q++) {
                v = -1;
                p_glGetFramebufferAttachmentParameteriv(0x8D40, att[a], pn[q], &v);
                printf("ATTACHMENT 0x%x 0x%x = %d error 0x%x\n", att[a], pn[q], v, p_glGetError());
            }
        p_glGetIntegerv(0x821D /* GL_NUM_EXTENSIONS */, &n);
        for (GLint i = 0; i < n; i++) printf("EXT %s\n", p_glGetStringi(0x1F03, i));
        printf("extensions %d\n", n);
        return 0;
    }
    GLuint tex = 0, fbo = 0;
    if (!strcmp(mode, "fbo")) {
        p_glGenTextures(1, &tex);
        p_glBindTexture(GL_TEXTURE_RECTANGLE, tex);
        p_glTexImage2D(GL_TEXTURE_RECTANGLE, 0, GL_RGBA8, 2048, 2048, 0, GL_RGBA, GL_UNSIGNED_BYTE, 0);
        p_glGenFramebuffers(1, &fbo);
        p_glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        p_glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_RECTANGLE, tex, 0);
        printf("fbo status 0x%x\n", p_glCheckFramebufferStatus(GL_FRAMEBUFFER));
    }
    for (int f = 0; f < frames; f++) {
        SDL_Event e;
        while (SDL_PollEvent(&e)) if (e.type == SDL_QUIT) return 0;
        if (fs) {
            int dw = 0, dh = 0;
            SDL_GL_GetDrawableSize(win, &dw, &dh);
            if (dw != w || dh != h) printf("drawable %dx%d at frame %d\n", dw, dh, f);
            if (dw > 0 && dh > 0 && dw <= 2048 && dh <= 2048) w = dw, h = dh;
        }
        p_glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        p_glViewport(0, 0, w, h);
        p_glClearColor(0.1f, 0.2f, 0.6f, 1);
        p_glClear(GL_COLOR_BUFFER_BIT);
        if (fbo) {
            p_glBegin(GL_QUADS);
            p_glColor3f(0.2f, 0.9f, 0.2f);
            p_glVertex2f(-0.5f, -0.5f); p_glVertex2f(0.5f, -0.5f);
            p_glVertex2f(0.5f, 0.5f); p_glVertex2f(-0.5f, 0.5f);
            p_glEnd();
            p_glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo);
            p_glBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0);
            p_glBlitFramebuffer(0, 0, w, h, 0, 0, w, h, GL_COLOR_BUFFER_BIT, GL_LINEAR);
            p_glBindFramebuffer(GL_FRAMEBUFFER, 0);
        }
        if (f == 0 || f == 120) {
            unsigned char px[4] = {0};
            p_glReadPixels(w / 2, h / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
            printf("centre %d %d %d error 0x%x\n", px[0], px[1], px[2], p_glGetError());
        }
        SDL_GL_SwapWindow(win);
        if (f % 60 == 0) { printf("frame %d\n", f); fflush(stdout); }
    }
    return 0;
}
