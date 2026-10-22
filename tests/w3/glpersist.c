/* ARB_buffer_storage persistent mappings written by the guest, as wined3d
 * writes the 64 MiB chunks it suballocates every Direct3D buffer from
 * (tests/w3/glpersist.sh). A persistent mapping is never unmapped and a
 * coherent one is never flushed, so the GL layer has to find what changed
 * itself: before the fix nothing written through one reached the host and
 * Solitaire drew its felt but no cards. A 4.4 core context, a 4 MiB colour
 * buffer (glBufferStorage, mapped once for the whole run) feeding a
 * full-screen triangle into an RGBA8 FBO, read back after each step:
 *   1. coherent: write A, glFlush                       -> A
 *   2. coherent: write B, SwapBuffers                   -> B
 *   3. coherent: write A, glFenceSync                   -> A
 *   4. a GPU copy of B into another range, a guest write elsewhere, glFlush:
 *      the copy's bytes are not sent back over        -> B from that range
 *   5. not coherent: write B, glMemoryBarrier(CLIENT_MAPPED_BUFFER_BARRIER) -> B
 *   6. not coherent, explicit flush: write A, glFlushMappedBufferRange -> A
 *   7. coherent: write B, draw at once; write A, draw again  -> B, then A
 *   8. coherent indices and colours, glDrawElements and
 *      glDrawRangeElementsBaseVertex at once                -> A, then B
 *   9. coherent with explicit flushes, as wined3d maps its chunks, each
 *      flush covering the whole Direct3D buffer (only the changed blocks
 *      need go): write A, flush a range around it at no block bound -> A;
 *      write B, flush the whole buffer -> B; flush it again -> B; a GPU
 *      copy of A under the map, B written again (the same bytes as the
 *      map held), flushed -> B; and a glBufferSubData of A the same way
 *      -> B
 *  10. a reader that is not a draw: glDispatchComputeIndirect's group
 *      counts written into such a mapping and flushed; the shader then
 *      stores a mark in a storage buffer, read back
 * Steps 1-6 put a flush, swap, fence, copy, barrier or explicit flush
 * between each write and the draw that reads it; 7 and 8 put nothing there:
 * a coherent write is visible to the next draw.
 * One "ok"/"FAIL" line a step; "glpersist ok" when all passed. Natively and
 * in the page. */
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>
#include <string.h>

#define WGL_CONTEXT_MAJOR_VERSION_ARB 0x2091
#define WGL_CONTEXT_MINOR_VERSION_ARB 0x2092
#define WGL_CONTEXT_FLAGS_ARB 0x2094
#define WGL_CONTEXT_PROFILE_MASK_ARB 0x9126
#define WGL_CONTEXT_CORE_PROFILE_BIT_ARB 0x0001
#define WGL_CONTEXT_FORWARD_COMPATIBLE_BIT_ARB 0x0002
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_VERTEX_SHADER 0x8B31
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_FRAMEBUFFER 0x8D40
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_ARRAY_BUFFER 0x8892
#define GL_COPY_READ_BUFFER 0x8F36
#define GL_COPY_WRITE_BUFFER 0x8F37
#define GL_DYNAMIC_DRAW 0x88E8
#define GL_MAP_WRITE_BIT 0x0002

#define GL_COPY_READ_BUFFER 0x8F36
#define GL_MAP_READ_BIT 0x0001
#define GL_MAP_FLUSH_EXPLICIT_BIT 0x0010
#define GL_MAP_UNSYNCHRONIZED_BIT 0x0020
#define GL_MAP_PERSISTENT_BIT 0x0040
#define GL_MAP_COHERENT_BIT 0x0080
#define GL_DYNAMIC_STORAGE_BIT 0x0100
#define GL_CLIENT_MAPPED_BUFFER_BARRIER_BIT 0x00004000
#define GL_SYNC_GPU_COMMANDS_COMPLETE 0x9117
#define GL_STATIC_DRAW 0x88E4

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok  " : "FAIL", name); fflush(stdout); if (!ok_) failures++; } while (0)

typedef HGLRC (WINAPI *wglCreateContextAttribsARB_t)(HDC, HGLRC, const int *);
#define PROC(ret, name, args) typedef ret (WINAPI *name##_t) args; static name##_t p##name;
PROC(GLuint, glCreateShader, (GLenum))
PROC(void, glShaderSource, (GLuint, GLsizei, const char *const *, const GLint *))
PROC(void, glCompileShader, (GLuint))
PROC(GLuint, glCreateProgram, (void))
PROC(void, glAttachShader, (GLuint, GLuint))
PROC(void, glBindAttribLocation, (GLuint, GLuint, const char *))
PROC(void, glLinkProgram, (GLuint))
PROC(void, glGetProgramiv, (GLuint, GLenum, GLint *))
PROC(void, glUseProgram, (GLuint))
PROC(void, glGenFramebuffers, (GLsizei, GLuint *))
PROC(void, glBindFramebuffer, (GLenum, GLuint))
PROC(void, glFramebufferTexture2D, (GLenum, GLenum, GLenum, GLuint, GLint))
PROC(void, glGenVertexArrays, (GLsizei, GLuint *))
PROC(void, glBindVertexArray, (GLuint))
PROC(void, glGenBuffers, (GLsizei, GLuint *))
PROC(void, glBindBuffer, (GLenum, GLuint))
PROC(void, glBufferData, (GLenum, ptrdiff_t, const void *, GLenum))
PROC(void, glBufferSubData, (GLenum, ptrdiff_t, ptrdiff_t, const void *))
PROC(void, glBufferStorage, (GLenum, ptrdiff_t, const void *, GLbitfield))
PROC(void *, glMapBufferRange, (GLenum, ptrdiff_t, ptrdiff_t, GLbitfield))
PROC(void, glFlushMappedBufferRange, (GLenum, ptrdiff_t, ptrdiff_t))
PROC(void, glCopyBufferSubData, (GLenum, GLenum, ptrdiff_t, ptrdiff_t, ptrdiff_t))
PROC(void, glVertexAttribPointer, (GLuint, GLint, GLenum, GLboolean, GLsizei, const void *))
PROC(void, glEnableVertexAttribArray, (GLuint))
PROC(void *, glFenceSync, (GLenum, GLbitfield))
PROC(void, glDeleteSync, (void *))
PROC(void, glMemoryBarrier, (GLbitfield))
PROC(void, glDrawRangeElementsBaseVertex, (GLenum, GLuint, GLuint, GLsizei, GLenum, const void *, GLint))
PROC(void, glDispatchComputeIndirect, (ptrdiff_t))
PROC(void, glBindBufferBase, (GLenum, GLuint, GLuint))
PROC(void, glGetBufferSubData, (GLenum, ptrdiff_t, ptrdiff_t, void *))
PROC(GLboolean, glUnmapBuffer, (GLenum))
#define LOAD(name) p##name = (name##_t)wglGetProcAddress(#name)

static const char *vs_src = "#version 150 core\nin vec2 p;\nin vec4 col;\nflat out vec4 c0;\n"
                            "void main() { c0 = col; gl_Position = vec4(p, 0.0, 1.0); }\n";
static const char *fs_src = "#version 150 core\nflat in vec4 c0;\nout vec4 c;\nvoid main() { c = c0; }\n";

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) { return DefWindowProcW(w, m, wp, lp); }

#define BUF_SIZE (4 << 20)
#define RANGE_OFF (3 << 20)  /* well into the buffer, as Solitaire's cards were */
#define RANGE2_OFF (1 << 20)
#define RANGE_SIZE 8192
static float colA[RANGE_SIZE / 4], colB[RANGE_SIZE / 4];

static void fill(float *dst, float r, float g, float b) {
    for (int i = 0; i < RANGE_SIZE / 16; i++) { dst[4 * i] = r; dst[4 * i + 1] = g; dst[4 * i + 2] = b; dst[4 * i + 3] = 1; }
}

/* Colours from `buf` at `off`; draw and say whether the middle is A (red)
 * or B (green). */
static void expect(const char *name, GLuint buf, long off, int want_b) {
    unsigned char px[4] = {0};
    char what[200];
    pglBindBuffer(GL_ARRAY_BUFFER, buf);
    pglVertexAttribPointer(1, 4, GL_FLOAT, GL_FALSE, 0, (const void *)(uintptr_t)off);
    pglEnableVertexAttribArray(1);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    GLenum err = glGetError();
    int is_a = px[0] > 200 && px[1] < 50 && px[2] < 50, is_b = px[0] < 50 && px[1] > 200 && px[2] < 50;
    snprintf(what, sizeof(what), "%s: reads %s (%u %u %u %u, error %#x)", name, want_b ? "B" : "A",
             px[0], px[1], px[2], px[3], err);
    CHECK(what, (want_b ? is_b : is_a) && err == GL_NO_ERROR);
}

int main(void) {
    WNDCLASSW wc = {0};
    wc.style = CS_OWNDC;
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gex_glpersist";
    RegisterClassW(&wc);
    HWND w = CreateWindowW(L"gex_glpersist", L"glpersist", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0, 0, 160, 160, 0, 0, wc.hInstance, 0);
    HDC dc = GetDC(w);
    PIXELFORMATDESCRIPTOR pfd = {sizeof(pfd), 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER, PFD_TYPE_RGBA, 32};
    SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd);
    HGLRC legacy = wglCreateContext(dc);
    wglMakeCurrent(dc, legacy);
    wglCreateContextAttribsARB_t create = (wglCreateContextAttribsARB_t)wglGetProcAddress("wglCreateContextAttribsARB");
    CHECK("wglCreateContextAttribsARB", create != NULL);
    if (!create) return 1;
    const int attribs[] = {WGL_CONTEXT_MAJOR_VERSION_ARB, 4, WGL_CONTEXT_MINOR_VERSION_ARB, 4,
                           WGL_CONTEXT_FLAGS_ARB, WGL_CONTEXT_FORWARD_COMPATIBLE_BIT_ARB,
                           WGL_CONTEXT_PROFILE_MASK_ARB, WGL_CONTEXT_CORE_PROFILE_BIT_ARB, 0};
    HGLRC core = create(dc, NULL, attribs);
    CHECK("4.4 core context", core != NULL);
    if (!core) return 1;
    wglMakeCurrent(dc, core);
    wglDeleteContext(legacy);
    printf("  GL_VERSION %s\n  GL_RENDERER %s\n", glGetString(GL_VERSION), glGetString(GL_RENDERER));
    LOAD(glCreateShader); LOAD(glShaderSource); LOAD(glCompileShader);
    LOAD(glCreateProgram); LOAD(glAttachShader); LOAD(glBindAttribLocation); LOAD(glLinkProgram);
    LOAD(glGetProgramiv); LOAD(glUseProgram); LOAD(glGenFramebuffers); LOAD(glBindFramebuffer);
    LOAD(glFramebufferTexture2D); LOAD(glGenVertexArrays); LOAD(glBindVertexArray); LOAD(glGenBuffers);
    LOAD(glBindBuffer); LOAD(glBufferData); LOAD(glBufferSubData); LOAD(glBufferStorage); LOAD(glMapBufferRange);
    LOAD(glFlushMappedBufferRange); LOAD(glCopyBufferSubData); LOAD(glVertexAttribPointer);
    LOAD(glEnableVertexAttribArray); LOAD(glFenceSync); LOAD(glDeleteSync); LOAD(glMemoryBarrier);
    LOAD(glDrawRangeElementsBaseVertex); LOAD(glDispatchComputeIndirect); LOAD(glBindBufferBase); LOAD(glGetBufferSubData); LOAD(glUnmapBuffer);
    CHECK("entry points", pglBufferStorage && pglMapBufferRange && pglFlushMappedBufferRange &&
                          pglFenceSync && pglMemoryBarrier && pglCopyBufferSubData);
    if (failures) return 1;

    GLint st = 0;
    GLuint prog = pglCreateProgram(), vs = pglCreateShader(GL_VERTEX_SHADER), fs = pglCreateShader(GL_FRAGMENT_SHADER);
    pglShaderSource(vs, 1, &vs_src, NULL);
    pglShaderSource(fs, 1, &fs_src, NULL);
    pglCompileShader(vs);
    pglCompileShader(fs);
    pglAttachShader(prog, vs);
    pglAttachShader(prog, fs);
    pglBindAttribLocation(prog, 0, "p");
    pglBindAttribLocation(prog, 1, "col");
    pglLinkProgram(prog);
    pglGetProgramiv(prog, GL_LINK_STATUS, &st);
    CHECK("program links", st);
    pglUseProgram(prog);

    GLuint tex = 0, fbo = 0, vao = 0, pos = 0, coh = 0, nc = 0, ex = 0, src = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 64, 64, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    pglGenFramebuffers(1, &fbo);
    pglBindFramebuffer(GL_FRAMEBUFFER, fbo);
    pglFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    glViewport(0, 0, 64, 64);

    const float tri[] = {-1, -1, 3, -1, -1, 3};
    pglGenVertexArrays(1, &vao);
    pglBindVertexArray(vao);
    pglGenBuffers(1, &pos);
    pglBindBuffer(GL_ARRAY_BUFFER, pos);
    pglBufferData(GL_ARRAY_BUFFER, sizeof(tri), tri, GL_STATIC_DRAW);
    pglVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
    pglEnableVertexAttribArray(0);

    fill(colA, 1, 0, 0);
    fill(colB, 0, 1, 0);
    pglGenBuffers(1, &src);
    pglBindBuffer(GL_COPY_READ_BUFFER, src);
    pglBufferData(GL_COPY_READ_BUFFER, RANGE_SIZE, colB, GL_STATIC_DRAW);

    /* Coherent, mapped the way wined3d maps its chunks. */
    const GLbitfield store = GL_MAP_READ_BIT | GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT | GL_MAP_COHERENT_BIT |
                             GL_DYNAMIC_STORAGE_BIT;
    pglGenBuffers(1, &coh);
    pglBindBuffer(GL_ARRAY_BUFFER, coh);
    pglBufferStorage(GL_ARRAY_BUFFER, BUF_SIZE, NULL, store);
    unsigned char *m = pglMapBufferRange(GL_ARRAY_BUFFER, 0, BUF_SIZE,
                                         GL_MAP_READ_BIT | GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT | GL_MAP_COHERENT_BIT);
    CHECK("coherent persistent map", m != NULL);
    if (!m) return 1;

    memcpy(m + RANGE_OFF, colA, RANGE_SIZE);
    glFlush();
    expect("1. coherent write, glFlush", coh, RANGE_OFF, 0);

    memcpy(m + RANGE_OFF, colB, RANGE_SIZE);
    SwapBuffers(dc);
    expect("2. coherent write, SwapBuffers", coh, RANGE_OFF, 1);

    memcpy(m + RANGE_OFF, colA, RANGE_SIZE);
    void *sync = pglFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
    expect("3. coherent write, glFenceSync", coh, RANGE_OFF, 0);
    if (sync) pglDeleteSync(sync);

    /* The device's write lands in a range the guest copy still holds zeros
     * for; publishing the guest's other write must not send those back. */
    pglBindBuffer(GL_COPY_WRITE_BUFFER, coh);
    pglCopyBufferSubData(GL_COPY_READ_BUFFER, GL_COPY_WRITE_BUFFER, 0, RANGE2_OFF, RANGE_SIZE);
    memcpy(m + RANGE_OFF, colB, RANGE_SIZE);
    glFlush();
    expect("4. GPU copy kept across a publish", coh, RANGE2_OFF, 1);
    expect("4. and the guest's write beside it", coh, RANGE_OFF, 1);

    /* Not coherent: a barrier publishes. */
    pglGenBuffers(1, &nc);
    pglBindBuffer(GL_ARRAY_BUFFER, nc);
    pglBufferStorage(GL_ARRAY_BUFFER, BUF_SIZE, NULL, GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT | GL_DYNAMIC_STORAGE_BIT);
    unsigned char *n = pglMapBufferRange(GL_ARRAY_BUFFER, 0, BUF_SIZE, GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT);
    CHECK("non-coherent persistent map", n != NULL);
    if (n) {
        memcpy(n + RANGE_OFF, colB, RANGE_SIZE);
        pglMemoryBarrier(GL_CLIENT_MAPPED_BUFFER_BARRIER_BIT);
        expect("5. non-coherent write, glMemoryBarrier", nc, RANGE_OFF, 1);
    }

    /* Not coherent, explicit flush: the range named. */
    pglGenBuffers(1, &ex);
    pglBindBuffer(GL_ARRAY_BUFFER, ex);
    pglBufferStorage(GL_ARRAY_BUFFER, BUF_SIZE, NULL, GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT | GL_DYNAMIC_STORAGE_BIT);
    unsigned char *e = pglMapBufferRange(GL_ARRAY_BUFFER, 0, BUF_SIZE,
                                         GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT | GL_MAP_FLUSH_EXPLICIT_BIT);
    CHECK("explicit-flush persistent map", e != NULL);
    if (e) {
        memcpy(e + RANGE_OFF, colA, RANGE_SIZE);
        pglFlushMappedBufferRange(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE);
        expect("6. explicit flush", ex, RANGE_OFF, 0);
    }

    /* Coherent, and nothing between the write and the draw. */
    memcpy(m + RANGE_OFF, colB, RANGE_SIZE);
    expect("7. coherent write, then a draw", coh, RANGE_OFF, 1);
    memcpy(m + RANGE_OFF, colA, RANGE_SIZE);
    expect("7. another write, then a draw", coh, RANGE_OFF, 0);

    /* Indexed: the indices and the colours both in the coherent buffer,
     * written just before each draw. The colours go at vertices 100..102
     * (named by the indices, then by base vertex 100), the indices well
     * past them. */
    {
        const long idx_off = (2 << 20) + 64, col_off = RANGE2_OFF + RANGE_SIZE;
        unsigned short idx[3] = {100, 101, 102};
        unsigned char px[4] = {0};
        char what[200];
        pglBindBuffer(GL_ARRAY_BUFFER, coh);
        pglVertexAttribPointer(1, 4, GL_FLOAT, GL_FALSE, 0, (const void *)(uintptr_t)col_off);
        pglEnableVertexAttribArray(1);
        /* glDrawElements reads position 100..102 too: put the triangle there. */
        GLuint pos2 = 0;
        float tri2[103 * 2] = {0};
        memcpy(tri2 + 200, tri, sizeof(tri));
        pglGenBuffers(1, &pos2);
        pglBindBuffer(GL_ARRAY_BUFFER, pos2);
        pglBufferData(GL_ARRAY_BUFFER, sizeof(tri2), tri2, GL_STATIC_DRAW);
        pglVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
        pglBindBuffer(0x8893 /* GL_ELEMENT_ARRAY_BUFFER */, coh);
        for (int step = 0; step < 2; step++) {
            memcpy(m + col_off + 100 * 16, step ? colB : colA, 3 * 16);
            if (step) {
                unsigned short zero[3] = {0, 1, 2};
                memcpy(m + idx_off, zero, sizeof(zero));
            } else {
                memcpy(m + idx_off, idx, sizeof(idx));
            }
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            if (step)
                pglDrawRangeElementsBaseVertex(GL_TRIANGLES, 0, 2, 3, GL_UNSIGNED_SHORT,
                                               (const void *)(uintptr_t)idx_off, 100);
            else
                glDrawElements(GL_TRIANGLES, 3, GL_UNSIGNED_SHORT, (const void *)(uintptr_t)idx_off);
            glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
            GLenum err = glGetError();
            int ok = step ? (px[0] < 50 && px[1] > 200 && px[2] < 50) : (px[0] > 200 && px[1] < 50 && px[2] < 50);
            snprintf(what, sizeof(what), "8. coherent indices and colours, %s: reads %s (%u %u %u %u, error %#x)",
                     step ? "glDrawRangeElementsBaseVertex" : "glDrawElements", step ? "B" : "A",
                     px[0], px[1], px[2], px[3], err);
            CHECK(what, ok && err == GL_NO_ERROR);
        }
    }

    /* Coherent, explicitly flushed, as wined3d maps its chunks: an unmap of
     * a Direct3D buffer flushes all of the buffer's range, whatever was
     * written. */
    {
        GLuint srcA = 0, wf[2] = {0};
        /* Step 8 left the positions at vertices 100..102: back to the
           triangle at 0..2. */
        pglBindBuffer(GL_ARRAY_BUFFER, pos);
        pglVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
        pglGenBuffers(1, &srcA);
        pglBindBuffer(GL_COPY_READ_BUFFER, srcA);
        pglBufferData(GL_COPY_READ_BUFFER, RANGE_SIZE, colA, GL_STATIC_DRAW);
        const GLbitfield wstore = GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT | GL_MAP_COHERENT_BIT | GL_DYNAMIC_STORAGE_BIT;
        const GLbitfield wmap = GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT | GL_MAP_COHERENT_BIT |
                                GL_MAP_FLUSH_EXPLICIT_BIT | GL_MAP_UNSYNCHRONIZED_BIT;
        pglGenBuffers(2, wf);
        for (int k = 0; k < 2; k++) {
            pglBindBuffer(GL_ARRAY_BUFFER, wf[k]);
            pglBufferStorage(GL_ARRAY_BUFFER, BUF_SIZE, NULL, wstore);
            unsigned char *f = pglMapBufferRange(GL_ARRAY_BUFFER, 0, BUF_SIZE, wmap);
            CHECK("coherent explicit-flush persistent map", f != NULL);
            if (!f) break;
            memcpy(f + RANGE_OFF, colA, RANGE_SIZE);
            pglFlushMappedBufferRange(GL_ARRAY_BUFFER, RANGE_OFF - 6000, RANGE_SIZE + 9000);
            expect("9. explicit flush around the write", wf[k], RANGE_OFF, 0);
            pglBindBuffer(GL_ARRAY_BUFFER, wf[k]);
            memcpy(f + RANGE_OFF, colB, RANGE_SIZE);
            pglFlushMappedBufferRange(GL_ARRAY_BUFFER, 0, BUF_SIZE);
            expect("9. a write, the whole buffer flushed", wf[k], RANGE_OFF, 1);
            pglBindBuffer(GL_ARRAY_BUFFER, wf[k]);
            pglFlushMappedBufferRange(GL_ARRAY_BUFFER, 0, BUF_SIZE);
            expect("9. the whole buffer flushed again", wf[k], RANGE_OFF, 1);
            /* The host's copy changed under the map (a GPU copy, or the
             * application's glBufferSubData); the write puts back the bytes
             * the map already held, and the flush must still send them. */
            if (k == 0) {
                pglBindBuffer(GL_COPY_WRITE_BUFFER, wf[k]);
                pglCopyBufferSubData(GL_COPY_READ_BUFFER, GL_COPY_WRITE_BUFFER, 0, RANGE_OFF, RANGE_SIZE);
            } else {
                pglBindBuffer(GL_ARRAY_BUFFER, wf[k]);
                pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colA);
            }
            expect(k ? "9. glBufferSubData under the map" : "9. GPU copy under the map", wf[k], RANGE_OFF, 0);
            pglBindBuffer(GL_ARRAY_BUFFER, wf[k]);
            memcpy(f + RANGE_OFF, colB, RANGE_SIZE);
            pglFlushMappedBufferRange(GL_ARRAY_BUFFER, 0, BUF_SIZE);
            expect(k ? "9. the same bytes rewritten after glBufferSubData, flushed"
                     : "9. the same bytes rewritten after a GPU copy, flushed", wf[k], RANGE_OFF, 1);
        }
    }

    /* 10. Not a draw: the indirect dispatch's group counts, in a coherent
     * explicitly flushed mapping, reach the host before the dispatch. */
    if (pglDispatchComputeIndirect) {
        static const char *cs_src = "#version 430\nlayout(local_size_x = 1) in;\n"
                                    "layout(std430, binding = 0) buffer M { uint mark; };\n"
                                    "void main() { mark = 0x600du; }\n";
        GLuint cs = pglCreateShader(0x91B9 /* GL_COMPUTE_SHADER */), cp = pglCreateProgram(), ind = 0, ssbo = 0;
        pglShaderSource(cs, 1, &cs_src, NULL);
        pglCompileShader(cs);
        pglAttachShader(cp, cs);
        pglLinkProgram(cp);
        GLint ok = 0;
        pglGetProgramiv(cp, GL_LINK_STATUS, &ok);
        CHECK("10. compute program links", ok);
        const unsigned zero = 0;
        pglGenBuffers(1, &ssbo);
        pglBindBuffer(0x90D2 /* GL_SHADER_STORAGE_BUFFER */, ssbo);
        pglBufferData(0x90D2, 4, &zero, GL_DYNAMIC_DRAW);
        pglBindBufferBase(0x90D2, 0, ssbo);
        pglGenBuffers(1, &ind);
        pglBindBuffer(0x90EE /* GL_DISPATCH_INDIRECT_BUFFER */, ind);
        pglBufferStorage(0x90EE, 4096, NULL, GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT | GL_MAP_COHERENT_BIT | GL_DYNAMIC_STORAGE_BIT);
        unsigned *g = pglMapBufferRange(0x90EE, 0, 4096, GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT |
                                        GL_MAP_COHERENT_BIT | GL_MAP_FLUSH_EXPLICIT_BIT);
        CHECK("10. indirect buffer mapped", g != NULL);
        if (g && ok) {
            g[64] = 1; g[65] = 1; g[66] = 1;
            pglFlushMappedBufferRange(0x90EE, 0, 4096);
            pglUseProgram(cp);
            pglDispatchComputeIndirect(256);
            pglMemoryBarrier(0x00002000 /* GL_BUFFER_UPDATE_BARRIER_BIT */);
            unsigned mark = 0;
            pglBindBuffer(0x90D2, ssbo);
            pglGetBufferSubData(0x90D2, 0, 4, &mark);
            char what[120];
            snprintf(what, sizeof(what), "10. glDispatchComputeIndirect reads flushed counts: mark %#x (error %#x)", mark, glGetError());
            CHECK(what, mark == 0x600du);
            pglUseProgram(prog);
        }
    }

    /* 11. Immutable storage without GL_DYNAMIC_STORAGE_BIT, written through a
       plain write mapping: the unmap must still reach the host's copy. */
    {
        GLuint b = 0;
        unsigned got[4] = {0};
        pglGenBuffers(1, &b);
        pglBindBuffer(0x8F36 /* GL_COPY_READ_BUFFER */, b);
        pglBufferStorage(0x8F36, 4096, NULL, GL_MAP_WRITE_BIT);
        unsigned *w = pglMapBufferRange(0x8F36, 0, 4096, GL_MAP_WRITE_BIT | 0x0004 /* INVALIDATE_RANGE */);
        CHECK("11. non-dynamic storage mapped", w != NULL);
        if (w) {
            for (int i = 0; i < 1024; i++) w[i] = 0x51000000u + i;
            pglUnmapBuffer(0x8F36);
            pglGetBufferSubData(0x8F36, 1000 * 4, sizeof(got), got);
            char what[120];
            snprintf(what, sizeof(what), "11. non-dynamic storage keeps a mapping's writes: %#x (error %#x)", got[0], glGetError());
            CHECK(what, got[0] == 0x51000000u + 1000 && got[3] == 0x51000000u + 1003);
        }
    }

    if (failures) { printf("glpersist FAIL (%d)\n", failures); return 1; }
    printf("glpersist ok\n");
    fflush(stdout);
    /* "hold": stay up, so a page run's harness can read the console. */
    if (strstr(GetCommandLineA(), "hold")) Sleep(60000);
    return 0;
}
