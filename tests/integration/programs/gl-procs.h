/* Entry points that must be resolved rather than linked.
 *
 * Mesa's non-glvnd libGL does not contain the direct-state-access or
 * internalformat-query entry points at all -- not hidden, absent -- because
 * that symbol list (src/glx/libgl-symbols.txt, 1374 names) predates them.
 * They are reachable only through glXGetProcAddress/eglGetProcAddress, which
 * is the mechanism GL intends and what every real toolkit uses.
 *
 * A probe that calls them directly links fine against the system's glvnd
 * libGL, which dispatches everything, and then dies at load against our own
 * build with "undefined symbol" after printing part of its output.
 * tests/gates/probe-suite.sh detects that and reports UNLINKABLE + NORESULT --
 * which is honest, but it means the probe never runs against the driver under
 * test at all. Four probes were in that state, 77 cases between them.
 *
 * Include this after <GL/glcorearb.h>, call gex_load_procs() once the context
 * is current, and the calls in the probe body need no other change.
 */
#ifndef GEX_GL_PROCS_H
#define GEX_GL_PROCS_H

#include <EGL/egl.h>
#include <stdio.h>

/* One line per entry point: the PFN type glcorearb.h already declares, and the
   name. Add to this list as probes need more. */
#define GEX_GL_PROC_LIST \
    GEX_GL_PROC(PFNGLGETINTERNALFORMATIVPROC, glGetInternalformativ) \
    GEX_GL_PROC(PFNGLGETINTERNALFORMATI64VPROC, glGetInternalformati64v) \
    GEX_GL_PROC(PFNGLGETNAMEDBUFFERSUBDATAPROC, glGetNamedBufferSubData) \
    GEX_GL_PROC(PFNGLINVALIDATENAMEDFRAMEBUFFERSUBDATAPROC, glInvalidateNamedFramebufferSubData) \
    GEX_GL_PROC(PFNGLVERTEXARRAYVERTEXBUFFERSPROC, glVertexArrayVertexBuffers) \
    GEX_GL_PROC(PFNGLGETTEXTURESUBIMAGEPROC, glGetTextureSubImage) \
    GEX_GL_PROC(PFNGLCREATEBUFFERSPROC, glCreateBuffers) \
    GEX_GL_PROC(PFNGLNAMEDBUFFERSTORAGEPROC, glNamedBufferStorage) \
    GEX_GL_PROC(PFNGLNAMEDBUFFERDATAPROC, glNamedBufferData) \
    GEX_GL_PROC(PFNGLNAMEDBUFFERSUBDATAPROC, glNamedBufferSubData) \
    GEX_GL_PROC(PFNGLGETNAMEDBUFFERPARAMETERIVPROC, glGetNamedBufferParameteriv) \
    GEX_GL_PROC(PFNGLGETNAMEDBUFFERPARAMETERI64VPROC, glGetNamedBufferParameteri64v) \
    GEX_GL_PROC(PFNGLMAPNAMEDBUFFERRANGEPROC, glMapNamedBufferRange) \
    GEX_GL_PROC(PFNGLUNMAPNAMEDBUFFERPROC, glUnmapNamedBuffer) \
    GEX_GL_PROC(PFNGLBINDTEXTUREUNITPROC, glBindTextureUnit) \
    GEX_GL_PROC(PFNGLCHECKNAMEDFRAMEBUFFERSTATUSPROC, glCheckNamedFramebufferStatus) \
    GEX_GL_PROC(PFNGLCLEARNAMEDFRAMEBUFFERFVPROC, glClearNamedFramebufferfv) \
    GEX_GL_PROC(PFNGLCREATEFRAMEBUFFERSPROC, glCreateFramebuffers) \
    GEX_GL_PROC(PFNGLCREATETEXTURESPROC, glCreateTextures) \
    GEX_GL_PROC(PFNGLCREATEVERTEXARRAYSPROC, glCreateVertexArrays) \
    GEX_GL_PROC(PFNGLENABLEVERTEXARRAYATTRIBPROC, glEnableVertexArrayAttrib) \
    GEX_GL_PROC(PFNGLGETNAMEDFRAMEBUFFERATTACHMENTPARAMETERIVPROC, glGetNamedFramebufferAttachmentParameteriv) \
    GEX_GL_PROC(PFNGLGETTEXTURELEVELPARAMETERIVPROC, glGetTextureLevelParameteriv) \
    GEX_GL_PROC(PFNGLGETVERTEXARRAYINDEXEDIVPROC, glGetVertexArrayIndexediv) \
    GEX_GL_PROC(PFNGLNAMEDFRAMEBUFFERTEXTUREPROC, glNamedFramebufferTexture) \
    GEX_GL_PROC(PFNGLTEXTUREPARAMETERIPROC, glTextureParameteri) \
    GEX_GL_PROC(PFNGLTEXTURESTORAGE2DPROC, glTextureStorage2D) \
    GEX_GL_PROC(PFNGLTEXTURESUBIMAGE2DPROC, glTextureSubImage2D) \
    GEX_GL_PROC(PFNGLVERTEXARRAYATTRIBBINDINGPROC, glVertexArrayAttribBinding) \
    GEX_GL_PROC(PFNGLVERTEXARRAYATTRIBFORMATPROC, glVertexArrayAttribFormat) \
    GEX_GL_PROC(PFNGLVERTEXARRAYVERTEXBUFFERPROC, glVertexArrayVertexBuffer)

#define GEX_GL_PROC(type, name) static type gex_p_##name;
GEX_GL_PROC_LIST
#undef GEX_GL_PROC

/* Resolve every name in the list. Returns the number that could not be found,
   and says which -- a probe that silently ran with a null pointer would
   segfault somewhere unrelated. */
static int gex_load_procs(void) {
    int missing = 0;
#define GEX_GL_PROC(type, name)                                             \
    gex_p_##name = (type)eglGetProcAddress(#name);                          \
    if (!gex_p_##name) {                                                    \
        fprintf(stderr, "INFO unresolved entry point: %s\n", #name);        \
        ++missing;                                                          \
    }
    GEX_GL_PROC_LIST
#undef GEX_GL_PROC
    return missing;
}

/* The call sites need no change. Defined after the loader so the loader's own
   use of the real names is not rewritten.
 *
 * This is the same list twice, which is the shape that has bitten this project
 * repeatedly -- a name present in one copy and absent from the other is a
 * silent disagreement. Here both mismatches are caught before a measurement
 * can happen, which is why it is tolerable: a #define with no list entry
 * refers to a gex_p_ variable that was never declared, so the probe does not
 * compile; a list entry with no #define leaves the call site bound to the real
 * symbol, so the probe does not link against our own libGL, which is the
 * UNLINKABLE this header exists to remove. Both blocks are generated from one
 * list -- regenerate rather than hand-edit.
 */

#define glGetInternalformativ                      gex_p_glGetInternalformativ
#define glGetInternalformati64v                    gex_p_glGetInternalformati64v
#define glGetNamedBufferSubData                    gex_p_glGetNamedBufferSubData
#define glInvalidateNamedFramebufferSubData        gex_p_glInvalidateNamedFramebufferSubData
#define glVertexArrayVertexBuffers                 gex_p_glVertexArrayVertexBuffers
#define glGetTextureSubImage                       gex_p_glGetTextureSubImage
#define glCreateBuffers                            gex_p_glCreateBuffers
#define glNamedBufferStorage                       gex_p_glNamedBufferStorage
#define glNamedBufferData                          gex_p_glNamedBufferData
#define glNamedBufferSubData                       gex_p_glNamedBufferSubData
#define glGetNamedBufferParameteriv                gex_p_glGetNamedBufferParameteriv
#define glGetNamedBufferParameteri64v              gex_p_glGetNamedBufferParameteri64v
#define glMapNamedBufferRange                      gex_p_glMapNamedBufferRange
#define glUnmapNamedBuffer                         gex_p_glUnmapNamedBuffer
#define glBindTextureUnit                          gex_p_glBindTextureUnit
#define glCheckNamedFramebufferStatus              gex_p_glCheckNamedFramebufferStatus
#define glClearNamedFramebufferfv                  gex_p_glClearNamedFramebufferfv
#define glCreateFramebuffers                       gex_p_glCreateFramebuffers
#define glCreateTextures                           gex_p_glCreateTextures
#define glCreateVertexArrays                       gex_p_glCreateVertexArrays
#define glEnableVertexArrayAttrib                  gex_p_glEnableVertexArrayAttrib
#define glGetNamedFramebufferAttachmentParameteriv gex_p_glGetNamedFramebufferAttachmentParameteriv
#define glGetTextureLevelParameteriv               gex_p_glGetTextureLevelParameteriv
#define glGetVertexArrayIndexediv                  gex_p_glGetVertexArrayIndexediv
#define glNamedFramebufferTexture                  gex_p_glNamedFramebufferTexture
#define glTextureParameteri                        gex_p_glTextureParameteri
#define glTextureStorage2D                         gex_p_glTextureStorage2D
#define glTextureSubImage2D                        gex_p_glTextureSubImage2D
#define glVertexArrayAttribBinding                 gex_p_glVertexArrayAttribBinding
#define glVertexArrayAttribFormat                  gex_p_glVertexArrayAttribFormat
#define glVertexArrayVertexBuffer                  gex_p_glVertexArrayVertexBuffer

#endif /* GEX_GL_PROCS_H */
