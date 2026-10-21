#include <GL/gl.h>
#include <GL/glext.h>
#include <string.h>
#include <windows.h>
static const char *volatile anchor = "STATIC";
int entry(void) {
    HWND w = CreateWindowExA(0, anchor, "WGL probe", 0, 0, 0, 64, 64, 0, 0, 0, 0);
    if (!w) {
        return 1;
    }
    HDC dc = GetDC(w);
    PIXELFORMATDESCRIPTOR requested = { 0 }, actual = { 0 };
    requested.nSize = sizeof(requested);
    requested.nVersion = 1;
    requested.dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
    requested.iPixelType = PFD_TYPE_RGBA;
    requested.cColorBits = 32;
    int format = ChoosePixelFormat(dc, &requested);
    if (!format) {
        return 2;
    }
    if (DescribePixelFormat(dc, 0, 0, 0) != 1) {
        return 18;
    }
    if (!DescribePixelFormat(dc, format, sizeof(actual), &actual) || !(actual.dwFlags & PFD_SUPPORT_OPENGL)) {
        return 3;
    }
    if (!SetPixelFormat(dc, format, &actual) || GetPixelFormat(dc) != format || SetPixelFormat(dc, format, &actual)) {
        return 4;
    }
    HGLRC context = wglCreateContext(dc);
    if (!context || wglGetCurrentContext() || !wglMakeCurrent(dc, context)) {
        return 5;
    }
    if (wglGetCurrentContext() != context || wglGetCurrentDC() != dc) {
        return 6;
    }
    const char *version = (const char *)glGetString(GL_VERSION);
    if (!version || strlen(version) < 3) {
        return 7;
    }
    GLint max_texture = 0;
    glGetIntegerv(GL_MAX_TEXTURE_SIZE, &max_texture);
    if (max_texture < 64 || glGetError()) {
        return 8;
    }
    HMODULE library = LoadLibraryA("opengl32.dll");
    void(WINAPI * gen_textures)(GLsizei, GLuint *) = (void *)GetProcAddress(library, "glGenTextures");
    void(WINAPI * bind_texture)(GLenum, GLuint) = (void *)GetProcAddress(library, "glBindTexture");
    void(WINAPI * tex_image)(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, const void *) =
        (void *)GetProcAddress(library, "glTexImage2D");
    void(WINAPI * read_pixels)(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, void *) = (void *)GetProcAddress(library, "glReadPixels");
    void(WINAPI * clear_color)(GLfloat, GLfloat, GLfloat, GLfloat) = (void *)GetProcAddress(library, "glClearColor");
    void(WINAPI * clear)(GLbitfield) = (void *)GetProcAddress(library, "glClear");
    PFNGLGENFRAMEBUFFERSPROC gen_fbo = (void *)wglGetProcAddress("glGenFramebuffers");
    PFNGLBINDFRAMEBUFFERPROC bind_fbo = (void *)wglGetProcAddress("glBindFramebuffer");
    PFNGLFRAMEBUFFERTEXTURE2DPROC attach = (void *)wglGetProcAddress("glFramebufferTexture2D");
    PFNGLCHECKFRAMEBUFFERSTATUSPROC check_fbo = (void *)wglGetProcAddress("glCheckFramebufferStatus");
    if (!gen_textures || !bind_texture || !tex_image || !read_pixels || !clear_color || !clear || !gen_fbo || !bind_fbo || !attach ||
        !check_fbo) {
        return 13;
    }
    clear_color(1, 0, 0, 1);
    clear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    unsigned char window_pixel[4];
    read_pixels(1, 1, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, window_pixel);
    if (memcmp(window_pixel, (unsigned char[]){ 255, 0, 0, 255 }, 4) || glGetError()) {
        return 19;
    }
    if (!SwapBuffers(dc)) {
        return 30;
    }
    BOOL(WINAPI * swap)(HDC) = (void *)GetProcAddress(LoadLibraryA("gdi32.dll"), "SwapBuffers");
    clear_color(0, 0, 1, 1);
    clear(GL_COLOR_BUFFER_BIT);
    if (!swap || !swap(dc) || glGetError()) {
        return 31;
    }
    GLuint texture, fbo;
    gen_textures(1, &texture);
    bind_texture(GL_TEXTURE_2D, texture);
    unsigned char initial[4 * 4 * 4];
    memset(initial, 0, sizeof(initial));
    tex_image(GL_TEXTURE_2D, 0, GL_RGBA8, 4, 4, 0, GL_RGBA, GL_UNSIGNED_BYTE, initial);
    gen_fbo(1, &fbo);
    bind_fbo(GL_FRAMEBUFFER, fbo);
    attach(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
    if (check_fbo(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        return 14;
    }
    clear_color(.25f, .5f, .75f, 1);
    clear(GL_COLOR_BUFFER_BIT);
    unsigned char pixels[64];
    read_pixels(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    for (unsigned i = 0; i < 16; ++i) {
        if (memcmp(pixels + i * 4, (unsigned char[]){ 64, 128, 191, 255 }, 4)) {
            return 15;
        }
    }
    PFNGLCREATESHADERPROC create_shader = (void *)wglGetProcAddress("glCreateShader");
    PFNGLSHADERSOURCEPROC source = (void *)wglGetProcAddress("glShaderSource");
    PFNGLCOMPILESHADERPROC compile = (void *)wglGetProcAddress("glCompileShader");
    PFNGLGETSHADERIVPROC shader_query = (void *)wglGetProcAddress("glGetShaderiv");
    if (!create_shader || !source || !compile || !shader_query) {
        return 16;
    }
    const char *parts[] = { "#version 330 core\n", "void main(){gl_Position=vec4(0,0,0,1);}" };
    GLint lengths[] = { -1, 37 }, ok = 0;
    lengths[1] = strlen(parts[1]);
    GLuint shader = create_shader(GL_VERTEX_SHADER);
    source(shader, 2, parts, lengths);
    compile(shader);
    shader_query(shader, GL_COMPILE_STATUS, &ok);
    if (!ok || glGetError()) {
        return 17;
    }
    PFNGLGENBUFFERSPROC gen_buffers = (void *)wglGetProcAddress("glGenBuffers");
    PFNGLBINDBUFFERPROC bind_buffer = (void *)wglGetProcAddress("glBindBuffer");
    PFNGLBUFFERDATAPROC buffer_data = (void *)wglGetProcAddress("glBufferData");
    PFNGLMAPBUFFERRANGEPROC map = (void *)wglGetProcAddress("glMapBufferRange");
    PFNGLUNMAPBUFFERPROC unmap = (void *)wglGetProcAddress("glUnmapBuffer");
    PFNGLFLUSHMAPPEDBUFFERRANGEPROC flush = (void *)wglGetProcAddress("glFlushMappedBufferRange");
    PFNGLDELETEBUFFERSPROC delete_buffers = (void *)wglGetProcAddress("glDeleteBuffers");
    if (!gen_buffers || !bind_buffer || !buffer_data || !map || !unmap || !flush || !delete_buffers) {
        return 20;
    }
    GLuint buffer;
    unsigned char initial_buffer[16];
    for (unsigned i = 0; i < 16; ++i) {
        initial_buffer[i] = i;
    }
    gen_buffers(1, &buffer);
    bind_buffer(GL_ARRAY_BUFFER, buffer);
    buffer_data(GL_ARRAY_BUFFER, 16, initial_buffer, GL_DYNAMIC_DRAW);
    unsigned char *mapped = map(GL_ARRAY_BUFFER, 4, 8, GL_MAP_WRITE_BIT);
    if (!mapped) {
        return 21;
    }
    mapped[2] = 99;
    if (!unmap(GL_ARRAY_BUFFER)) {
        return 22;
    }
    mapped = map(GL_ARRAY_BUFFER, 0, 16, GL_MAP_READ_BIT);
    if (!mapped) {
        return 23;
    }
    for (unsigned i = 0; i < 16; ++i) {
        if (mapped[i] != (i == 6 ? 99 : i)) {
            return 24;
        }
    }
    if (!unmap(GL_ARRAY_BUFFER)) {
        return 25;
    }
    mapped = map(GL_ARRAY_BUFFER, 0, 16, GL_MAP_WRITE_BIT | GL_MAP_FLUSH_EXPLICIT_BIT);
    if (!mapped) {
        return 26;
    }
    mapped[8] = 77;
    mapped[9] = 88;
    flush(GL_ARRAY_BUFFER, 8, 1);
    if (!unmap(GL_ARRAY_BUFFER)) {
        return 27;
    }
    mapped = map(GL_ARRAY_BUFFER, 0, 16, GL_MAP_READ_BIT);
    if (!mapped || mapped[8] != 77 || mapped[9] != 9) {
        return 28;
    }
    if (!unmap(GL_ARRAY_BUFFER)) {
        return 29;
    }
    delete_buffers(1, &buffer);
    HGLRC(WINAPI * create_attribs)(HDC, HGLRC, const int *) = (void *)wglGetProcAddress("wglCreateContextAttribsARB");
    int attribs[] = { 0x2091, 3, 0x2092, 3, 0x9126, 1, 0 };
    HGLRC core = create_attribs ? create_attribs(dc, 0, attribs) : 0;
    if (!core || !wglMakeCurrent(dc, core)) {
        return 9;
    }
    if (!wglMakeCurrent(0, 0) || wglGetCurrentContext() || wglGetCurrentDC()) {
        return 10;
    }
    if (!wglDeleteContext(core) || wglDeleteContext(core) || !wglDeleteContext(context)) {
        return 11;
    }
    if (!ReleaseDC(w, dc) || !DestroyWindow(w)) {
        return 12;
    }
    OutputDebugStringA("PASS: PE32 WGL contexts, pixels, shader pointer arrays and mapped buffer coherence\n");
    return 0;
}
