/* Execute gex's API-level GL wire protocol against Mesa, in the browser.
 *
 * The guest's GL driver (guest/gl) sends GL calls down the wire; this decodes
 * them, maps the guest's object names onto the host's, and runs them through
 * Mesa's st/mesa frontend on one of two Gallium drivers:
 *
 *   GEX_MESA_WGPU   gex_wgpu, onto the browser's WebGPU
 *   otherwise       softpipe, the software reference
 *
 * Built by the gex-wasm target in the Mesa fork (third_party/mesa,
 * src/gallium/targets/gex-wasm/meson.build), which defines GEX_MESA_WASM.
 */
#if !defined(GEX_MESA_WASM)
#error "gexgl-mesa.cpp is built by Mesa's gex-wasm target, which defines GEX_MESA_WASM"
#endif
#define GL_GLEXT_PROTOTYPES
#include <GL/gl.h>
#include <GL/glext.h>

#include "frontend/api.h"
#include "frontend/sw_winsys.h"
#include "mesa/main/menums.h"
#include "mesa/glapi/glapi/glapi.h"
#include "mesa/state_tracker/st_context.h"
#include "pipe/p_state.h"
#if defined(GEX_MESA_WGPU)
/* Defined in the driver (C). Reported from the present path below. */
extern "C" unsigned gex_wgpu_draws_declined;
extern "C" unsigned gex_wgpu_draws_issued;
extern "C" unsigned gex_wgpu_declined_constant;
extern "C" unsigned gex_wgpu_declined_texture;
extern "C" unsigned gex_wgpu_declined_vertex;
extern "C" void gex_wgpu_report_draw_targets(void);
extern "C" const char *gex_wgpu_declined_last_why;
#endif

#if defined(GEX_MESA_WGPU)
/* The same Mesa frontend, driven onto the browser's WebGPU instead of a
 * software rasterizer.  Only the pipe_screen differs: st/mesa, NIR and this
 * decoder are identical, which is what makes softpipe a usable oracle for
 * this backend rather than a separate implementation. */
#include <webgpu/webgpu.h>

#include "gex_wgpu/gex_wgpu_public.h"
extern "C" WGPUDevice emscripten_webgpu_get_device(void);
#else
#include "softpipe/sp_public.h"
#include "sw/null/null_sw_winsys.h"
#endif
#include <emscripten/emscripten.h>
#include <emscripten/html5.h>

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <map>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <vector>

#include "../../../guest/gl/gexgl-pixel.h"


namespace {
template <typename Function>
Function mesa_proc(const char* name) {
    return reinterpret_cast<Function>(_mesa_glapi_get_proc_address(name));
}

GLint mesa_get_program_resource_location_index(GLuint program,
                                                GLenum interface_type,
                                                const GLchar* name) {
    using Function = GLint (*)(GLuint, GLenum, const GLchar*);
    static Function function =
        mesa_proc<Function>("glGetProgramResourceLocationIndex");
    return function(program, interface_type, name);
}

void mesa_patch_parameter_fv(GLenum pname, const GLfloat* values) {
    using Function = void (*)(GLenum, const GLfloat*);
    static Function function = mesa_proc<Function>("glPatchParameterfv");
    function(pname, values);
}

void mesa_get_internal_format_iv(GLenum target, GLenum internal_format,
                                 GLenum pname, GLsizei count, GLint* values) {
    using Function = void (*)(GLenum, GLenum, GLenum, GLsizei, GLint*);
    static Function function = mesa_proc<Function>("glGetInternalformativ");
    function(target, internal_format, pname, count, values);
}

#define glGetProgramResourceLocationIndex mesa_get_program_resource_location_index
#define glPatchParameterfv mesa_patch_parameter_fv
#define glGetInternalformativ mesa_get_internal_format_iv
#define GEX_GL_CALL(function, ...) \
    mesa_proc<decltype(&function)>(#function)(__VA_ARGS__)

enum Op : uint32_t {
    CONTEXT_CREATE = 1, SURFACE_RESIZE = 2, FINISH = 3, PRESENT = 4,
    SURFACE_ATTACH = 5, SURFACE_CURRENT = 6, SURFACE_DETACH = 7,
    CONTEXT_DESTROY = 8,
    CLEAR_COLOR = 16, CLEAR = 17, VIEWPORT = 18, ENABLE = 19, DISABLE = 20,
    BLEND_FUNC = 21, DEPTH_FUNC = 22, CULL_FACE = 23, FRONT_FACE = 24,
    SCISSOR = 25, PIXEL_STOREI = 26, CLEAR_DEPTH = 27, DEPTH_MASK = 28,
    HINT = 29, COLOR_MASK = 30, BLEND_EQUATION = 31,
    GEN_BUFFERS = 32, DELETE_BUFFERS = 33, BIND_BUFFER = 34,
    BUFFER_DATA = 35, BUFFER_SUB_DATA = 36, BLEND_FUNC_SEPARATE = 37,
    POLYGON_OFFSET = 38, LINE_WIDTH = 39, CLEAR_STENCIL = 40, DEPTH_RANGE = 41,
    BIND_BUFFER_BASE = 42, GEN_SAMPLERS = 43, DELETE_SAMPLERS = 44,
    BIND_SAMPLER = 45, SAMPLER_PARAMETER_I = 46, SAMPLER_PARAMETER_F = 47,
    CREATE_SHADER = 48, SHADER_SOURCE = 49, COMPILE_SHADER = 50,
    CREATE_PROGRAM = 51, ATTACH_SHADER = 52, BIND_ATTRIB_LOCATION = 53,
    LINK_PROGRAM = 54, USE_PROGRAM = 55, DELETE_SHADER = 56,
    DELETE_PROGRAM = 57, GET_UNIFORM_LOCATION = 58, GET_ATTRIB_LOCATION = 59,
    GET_SHADER_IV = 60, GET_PROGRAM_IV = 61, GET_SHADER_INFO_LOG = 62,
    GET_PROGRAM_INFO_LOG = 63,
    UNIFORM_1I = 64, UNIFORM_1F = 65, UNIFORM_2F = 66, UNIFORM_3F = 67,
    UNIFORM_4F = 68, UNIFORM_MATRIX4FV = 69, GET_ACTIVE_UNIFORM = 70,
    GET_ACTIVE_ATTRIB = 71, UNIFORM_1IV = 72, UNIFORM_1FV = 73,
    UNIFORM_2FV = 74, UNIFORM_3FV = 75, UNIFORM_4FV = 76,
    UNIFORM_2IV = 77, UNIFORM_3IV = 78, UNIFORM_4IV = 79,
    ENABLE_VERTEX_ATTRIB_ARRAY = 80, DISABLE_VERTEX_ATTRIB_ARRAY = 81,
    VERTEX_ATTRIB_POINTER = 82, GEN_VERTEX_ARRAYS = 83,
    DELETE_VERTEX_ARRAYS = 84, BIND_VERTEX_ARRAY = 85,
    UNIFORM_MATRIX2FV = 86, UNIFORM_MATRIX3FV = 87,
    DETACH_SHADER = 88, GET_ATTACHED_SHADERS = 89,
    GET_UNIFORM_BLOCK_INDEX = 90, UNIFORM_BLOCK_BINDING = 91,
    UNIFORM_2I = 92, UNIFORM_3I = 93, UNIFORM_4I = 94,
    GEN_TEXTURES = 96, DELETE_TEXTURES = 97, BIND_TEXTURE = 98,
    ACTIVE_TEXTURE = 99, TEX_PARAMETER_I = 100, TEX_IMAGE_2D = 101,
    TEX_SUB_IMAGE_2D = 102, GEN_FRAMEBUFFERS = 103, DELETE_FRAMEBUFFERS = 104,
    BIND_FRAMEBUFFER = 105, CHECK_FRAMEBUFFER_STATUS = 106,
    FRAMEBUFFER_TEXTURE_2D = 107, GEN_RENDERBUFFERS = 108,
    DELETE_RENDERBUFFERS = 109, BIND_RENDERBUFFER = 110, RENDERBUFFER_STORAGE = 111,
    DRAW_ARRAYS = 112, DRAW_ELEMENTS = 113, FRAMEBUFFER_RENDERBUFFER = 114,
    GENERATE_MIPMAP = 115, TEX_STORAGE_2D = 116, TEX_STORAGE_3D = 117,
    TEX_IMAGE_3D = 118, FRAMEBUFFER_TEXTURE_LAYER = 119,
    DRAW_BUFFERS = 120, VERTEX_ATTRIB_I_POINTER = 121,
    VERTEX_ATTRIB_DIVISOR = 122, DRAW_ARRAYS_INSTANCED = 123,
    DRAW_ELEMENTS_INSTANCED = 124, BLIT_FRAMEBUFFER = 125,
    VERTEX_ATTRIB_4F = 126, VERTEX_ATTRIB_I4I = 127,
    READ_PIXELS = 128, GET_STRING = 129, GET_INTEGERV = 130, GET_ERROR = 131,
    GET_STRING_I = 132, BLEND_COLOR = 133, CLEAR_BUFFER_FV = 134,
    CLEAR_BUFFER_IV = 135, CLEAR_BUFFER_UIV = 136, CLEAR_BUFFER_FI = 137,
    STENCIL_FUNC = 138, STENCIL_FUNC_SEPARATE = 139, STENCIL_MASK = 140,
    STENCIL_MASK_SEPARATE = 141, STENCIL_OP = 142,
    STENCIL_OP_SEPARATE = 143, BLEND_EQUATION_SEPARATE = 144,
    GET_UNIFORM_FV = 145, GET_UNIFORM_IV = 146, TEX_PARAMETER_F = 147,
    TEX_PARAMETER_FV = 148, TEX_PARAMETER_IV = 149,
    GEN_QUERIES = 150, DELETE_QUERIES = 151, IS_QUERY = 152,
    GEN_TRANSFORM_FEEDBACKS = 153, DELETE_TRANSFORM_FEEDBACKS = 154,
    BIND_TRANSFORM_FEEDBACK = 155, IS_TRANSFORM_FEEDBACK = 156,
    PAUSE_TRANSFORM_FEEDBACK = 157, RESUME_TRANSFORM_FEEDBACK = 158,
    GET_TRANSFORM_FEEDBACK_VARYING = 159, GET_SHADER_SOURCE = 160,
    GET_ACTIVE_UNIFORM_NAME = 161, GET_ACTIVE_UNIFORM_BLOCK_NAME = 162,
    GET_ACTIVE_UNIFORM_BLOCKIV = 163, GET_ACTIVE_UNIFORMSIV = 164,
    GET_UNIFORM_INDICES = 165,
    /* Fixed function; see crates/gex-gl/src/proto.rs. */
    MATRIX_MODE = 166, LOAD_IDENTITY = 167, PUSH_MATRIX = 168,
    POP_MATRIX = 169, TRANSLATEF = 170, ROTATEF = 171, FRUSTUM = 172,
    BEGIN = 173, END = 174, VERTEX3F = 175, NORMAL3F = 176,
    SHADE_MODEL = 177, LIGHTFV = 178, MATERIALFV = 179, GEN_LISTS = 180,
    NEW_LIST = 181, END_LIST = 182, CALL_LIST = 183, DELETE_LISTS = 184,
    /* Protocol 11: client arrays and fixed-function matrices. */
    ENABLE_CLIENT_STATE = 185, DISABLE_CLIENT_STATE = 186,
    VERTEX_POINTER = 187, COLOR_POINTER = 188, TEX_COORD_POINTER = 189,
    CLIENT_ACTIVE_TEXTURE = 190, ORTHO = 191, SCALEF = 192,
    LOAD_MATRIXF = 193, MULT_MATRIXF = 194, COLOR4F = 195, TEX_ENVI = 196,
    ALPHA_FUNC = 197,
    /* Protocol 12: the attribute stack. */
    PUSH_ATTRIB = 198, POP_ATTRIB = 199,
    /* A client-memory vertex array's bytes with its pointer command; see
     * crates/gex-gl/src/proto.rs. */
    CLIENT_ARRAY = 200,
};

struct Reader {
    const uint8_t* p;
    const uint8_t* end;
    bool ok = true;
    uint32_t u32() {
        if (end - p < 4) { ok = false; return 0; }
        uint32_t v; std::memcpy(&v, p, 4); p += 4; return v;
    }
    int32_t i32() { return static_cast<int32_t>(u32()); }
    uint64_t u64() {
        const uint64_t low = u32();
        const uint64_t high = u32();
        return low | (high << 32);
    }
    int64_t i64() { return static_cast<int64_t>(u64()); }
    float f32() { uint32_t u = u32(); float f; std::memcpy(&f, &u, 4); return f; }
    double f64() { uint64_t u = u64(); double f; std::memcpy(&f, &u, 8); return f; }
    const uint8_t* blob(uint32_t& n) {
        n = u32();
        if (!ok || static_cast<size_t>(end - p) < n) { ok = false; n = 0; return nullptr; }
        const uint32_t padded = n <= UINT32_MAX - 3u ? (n + 3u) & ~3u : 0;
        if (padded < n || static_cast<size_t>(end - p) < padded) {
            ok = false; n = 0; return nullptr;
        }
        const uint8_t* out = p; p += padded;
        return out;
    }
    std::string string() {
        uint32_t n; const uint8_t* s = blob(n);
        return s ? std::string(reinterpret_cast<const char*>(s), n) : std::string();
    }
};

std::vector<uint8_t> reply;
/* CLIENT_ARRAY's bytes, one copy per array slot. GL keeps the pointer, so the
 * bytes stay until the slot's next CLIENT_ARRAY replaces them -- which the
 * guest sends before every draw that reads a client array. */
std::unordered_map<uint32_t, std::vector<uint8_t>> client_arrays;
std::unordered_map<uint32_t, GLuint> buffers, textures, shaders, programs;
/* The head of each shader's source, kept only so a failed compile can show
 * what it was handed. A GLSL error names a line and column and nothing else,
 * and "0:1(1): syntax error, unexpected NEW_IDENTIFIER" is unreadable without
 * the line it is pointing at. Capped, because some of these are kilobytes. */
std::unordered_map<GLuint, std::string> shader_source_heads;
std::unordered_set<uint32_t> delete_pending_programs;
uint32_t current_guest_program = 0;
std::unordered_map<uint32_t, GLuint> vertex_arrays, framebuffers, renderbuffers;
std::unordered_map<uint32_t, GLuint> samplers;
std::unordered_map<uint32_t, GLuint> queries, transform_feedbacks;
std::unordered_map<uint32_t, GLuint> program_pipelines;
/* Surface size, remembered until the context that draws to it exists. Above
 * the backend blocks because a new drawable is built with it. */
int backend_width = 1;
int backend_height = 1;

/* What the *page* configured, before any client existed.
 *
 * `gex_gl_init` and `gex_gl_set_api` are asked once, by the embedder, and
 * what they set is per client from then on -- so a client that turns up
 * later has to inherit them rather than start from this file's defaults.
 * Without that the second GL application got a 1x1 drawable and an ES 3.0
 * context however the page had configured the device. */
int device_profile = static_cast<int>(API_OPENGLES2);
int device_major = 3;
int device_minor = 0;
int device_width = 1;
int device_height = 1;

#if !defined(GEX_MESA_WGPU)
struct sw_winsys* mesa_winsys = nullptr;
#endif
struct pipe_frontend_screen mesa_frontend_screen = {};
struct st_context* mesa_context = nullptr;

/* The default framebuffer.
 *
 * st/mesa reaches framebuffer 0 through a drawable the winsys owns, so a
 * context made current with none has no framebuffer 0 at all: every clear,
 * draw and read against it is GL_INVALID_FRAMEBUFFER_OPERATION.  The guest
 * renders to framebuffer 0 -- that is what its surface is -- so the backend
 * has to supply one.  Nothing here is a window; the attachments are plain
 * pipe resources of the size the transport was initialized with, which is
 * also what makes replay deterministic.
 */
struct gex_drawable {
    struct pipe_frontend_drawable base;
    struct st_visual visual;
    struct pipe_resource* attachments[ST_ATTACHMENT_COUNT];
    unsigned width;
    unsigned height;
};

/* One surface a client draws into: a window, or a pbuffer, or -- handle zero
 * -- the default one a client that never created a surface at all still has.
 *
 * There used to be a single drawable for the whole device, which is right up
 * until a client has two windows or two clients each have one: the second
 * window's frames went into the first's, and whichever surface attached last
 * owned them both. The guest names its surfaces now (`op::SURFACE_ATTACH`
 * carries the handle its EGL made up), so there is a drawable per handle and
 * a frame belongs to the window it was drawn for.
 *
 * `produced` and `displayed` are per surface for the same reason: a client
 * swapping window A must not be released by a frame of window B reaching the
 * screen. */
struct gex_surface {
    gex_drawable drawable{};
    /* Unique across the device, not just within a client: two clients both
     * call their first surface handle 1, and the page keeps one frame per
     * surface. */
    unsigned uid = 0;
    unsigned kind = 0;      /* surface::WAYLAND, surface::X, or none */
    unsigned id = 0;        /* wl_surface object id, or XID */
    unsigned token = 0;     /* the client's pid, as the compositor knows it */
    uint32_t produced = 0;
    uint32_t displayed = 0;
    unsigned generation = 0;
    /* Its frames come from the D3D device (gex_gl_external_frame). */
    bool external = false;
    /* The colour buffer of the frame last presented, held until the next
     * present: what the page shows. A resize drops the drawable's buffers
     * right after the present it follows (eglSwapBuffers notices the new
     * size then), before the page has shown that frame; without this a
     * client that draws only on a change (gpui after a window manager's
     * resize) never had a frame shown at all. */
    struct pipe_resource* shown = nullptr;
};

/* This client's surfaces, and which is current. `sur` is always the entry
 * `current_surface` names once a context exists; ordered, so the enumeration
 * the page reads is stable. */
std::map<uint32_t, gex_surface> surfaces;
uint32_t current_surface = 0;
gex_surface* sur = nullptr;
unsigned next_surface_uid = 1;

gex_drawable* drawable_of(struct pipe_frontend_drawable* drawable) {
    return reinterpret_cast<gex_drawable*>(drawable);
}

bool drawable_flush_front(struct st_context* st,
                          struct pipe_frontend_drawable* drawable,
                          enum st_attachment_type statt) {
    (void)st;
    (void)drawable;
    (void)statt;
    /* There is no window to copy to; the front buffer is the resource. */
    return true;
}

bool drawable_flush_swapbuffers(struct st_context* st,
                                struct pipe_frontend_drawable* drawable) {
    (void)st;
    (void)drawable;
    return true;
}

bool drawable_validate(struct st_context* st,
                       struct pipe_frontend_drawable* drawable,
                       const enum st_attachment_type* statts, unsigned count,
                       struct pipe_resource** out,
                       struct pipe_resource** resolve) {
    (void)st;
    gex_drawable* self = drawable_of(drawable);
    struct pipe_screen* screen = mesa_frontend_screen.screen;
    if (!screen) return false;

    for (unsigned i = 0; i < count; ++i) {
        const enum st_attachment_type statt = statts[i];
        if (statt < 0 || statt >= ST_ATTACHMENT_COUNT) return false;

        if (!self->attachments[statt]) {
            enum pipe_format format = PIPE_FORMAT_NONE;
            unsigned bind = 0;
            switch (statt) {
            case ST_ATTACHMENT_FRONT_LEFT:
            case ST_ATTACHMENT_BACK_LEFT:
                format = self->visual.color_format;
                /* No PIPE_BIND_DISPLAY_TARGET: nothing here is scanned out,
                 * and the null software winsys has no display target to give.
                 */
                bind = PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW;
                break;
            case ST_ATTACHMENT_DEPTH_STENCIL:
                format = self->visual.depth_stencil_format;
                bind = PIPE_BIND_DEPTH_STENCIL;
                break;
            default:
                break;
            }
            if (format == PIPE_FORMAT_NONE) {
                out[i] = nullptr;
                continue;
            }

            struct pipe_resource templ = {};
            templ.target = PIPE_TEXTURE_2D;
            templ.format = format;
            templ.width0 = self->width;
            templ.height0 = self->height;
            templ.depth0 = 1;
            templ.array_size = 1;
            templ.last_level = 0;
            templ.nr_samples = self->visual.samples;
            templ.nr_storage_samples = self->visual.samples;
            templ.bind = bind;
            self->attachments[statt] = screen->resource_create(screen, &templ);
            if (!self->attachments[statt]) return false;
        }
        /* The caller owns what it is handed, so hand it a reference. */
        out[i] = nullptr;
        pipe_resource_reference(&out[i], self->attachments[statt]);
    }
    if (resolve) *resolve = nullptr;
    return true;
}

/* The visual every one of this client's drawables is built with.
 *
 * Chosen once, in create_backend_context, and wanted again every time a new
 * surface turns up -- which now happens long after the context exists. */
struct st_visual backend_visual = {};

/* The drawable for a handle, made if this is the first time it is named.
 *
 * Handle zero is the surface a client that never created one still has: the
 * CTS and the replay harnesses draw into framebuffer 0 without ever calling
 * eglCreateWindowSurface, and they must keep working. */
gex_surface* surface_for(uint32_t handle) {
    auto found = surfaces.find(handle);
    if (found != surfaces.end()) return &found->second;
    gex_surface& made = surfaces[handle];
    made.uid = next_surface_uid++;
    made.drawable.visual = backend_visual;
    made.drawable.width = backend_width > 0 ? (unsigned)backend_width : 1;
    made.drawable.height = backend_height > 0 ? (unsigned)backend_height : 1;
    made.drawable.base.visual = &made.drawable.visual;
    made.drawable.base.fscreen = &mesa_frontend_screen;
    /* st/mesa keys its framebuffer cache on this identifier, so every
     * drawable needs its own: two surfaces both claiming ID 1 are one
     * framebuffer as far as st is concerned, which is the confusion this
     * whole table exists to end. */
    made.drawable.base.ID = handle + 1;
    made.drawable.base.stamp = 1;
    made.drawable.base.flush_front = drawable_flush_front;
    made.drawable.base.validate = drawable_validate;
    made.drawable.base.flush_swapbuffers = drawable_flush_swapbuffers;
    return &made;
}

/* Told to whatever is decoding for us, when it keeps anything per surface.
 * Defined per backend below; a no-op for the ones that do not. */
void note_surface_to_backend(unsigned uid);
/* This surface is gone: let go of any window canvas it was shown in.
 * Defined per backend below. */
void forget_window_canvas(unsigned uid);

/* Draw into this handle from here on: `op::SURFACE_CURRENT`, and what an
 * attach does as well. */
bool use_surface(uint32_t handle) {
    gex_surface* next = surface_for(handle);
    current_surface = handle;
    if (sur == next) return true;
    sur = next;
    /* Every flush from here belongs to this surface, not only the one that
     * presents it. */
    note_surface_to_backend(sur->uid);
    /* Before the context exists there is nothing to make current with; the
     * context creation that follows uses whatever is current then. */
    if (!mesa_context) return true;
    return st_api_make_current(mesa_context, &sur->drawable.base,
                               &sur->drawable.base);
}

/* Told to the page so it can drop the frame it keeps for a surface. Set by
 * the backend that has one; see `gex_gl_surface_gone`. */
uint32_t surface_gone_handle = 0;
unsigned surface_gone_generation = 0;

/* Everything a surface owns goes back: its colour and depth buffers, and the
 * page's copy of its last frame.
 *
 * Without this a closed window's buffers stayed allocated for as long as the
 * client lived, and -- worse -- the page went on drawing the frame it had
 * kept for a window that no longer existed. */
void drop_surface(uint32_t handle, bool repoint = true) {
    auto found = surfaces.find(handle);
    if (found == surfaces.end()) return;
    for (struct pipe_resource*& attachment : found->second.drawable.attachments)
        pipe_resource_reference(&attachment, nullptr);
    pipe_resource_reference(&found->second.shown, nullptr);
    /* And out of st/mesa's table of live drawables, so the next make-current
     * lets go of the framebuffer built on it. Left there, the entry outlived
     * the memory, and a later drawable at the same address with the same ID
     * (the same handle in the next client) was taken for the old one. */
    st_api_destroy_drawable(&found->second.drawable.base);
    const unsigned uid = found->second.uid;
    surfaces.erase(found);
    forget_window_canvas(uid);
    surface_gone_handle = uid;
    ++surface_gone_generation;
    if (current_surface != handle || !repoint) return;
    /* Whatever is left, or the default surface if nothing is: a context must
     * always have a framebuffer 0, and the client may well draw again. */
    sur = nullptr;
    use_surface(surfaces.empty() ? 0 : surfaces.begin()->first);
}
uint32_t produced = 0;
uint32_t displayed = 0;
/* Frames shown, by where they came from, across every client: the frame
 * report says which, since a D3D program's process may run GL as well. */
uint32_t displayed_gl = 0, displayed_d3d = 0;

/* One more frame shown. The first tells the page glue (gex-wgpu-split.js),
 * which then fetches and compiles Mesa's cold part in the background: from
 * here, not from a poll, which a workload reaching its first cold function
 * soon after its first frame could beat. */
void count_shown() {
    if (displayed++ == 0)
        MAIN_THREAD_ASYNC_EM_ASM({
            if (typeof Module['gexColdFirstFrame'] === 'function') Module['gexColdFirstFrame']();
        });
}
/* The frames a D3D window's last show left unshown: none, newest-first. */
uint32_t d3d_behind = 0;
GLuint default_vao = 0;
std::vector<GLenum> current_draw_buffers{GL_BACK};
GLfloat current_clear_color[4] = {0.f, 0.f, 0.f, 0.f};
GLfloat current_clear_depth = 1.f;
GLint current_clear_stencil = 0;
/* A negative count is legal to pass to the glGen and glDelete calls: GL answers
   GL_INVALID_VALUE, and packed negative-count coverage (KHR-GL32
   .buffer_objects.gen_buffers) checks exactly that. Read as signed and hand
   the negative straight to GL rather than reading it as unsigned and sizing a
   four-billion-element vector from it, which is an allocation failure that
   takes the whole runtime with it. Returns true when the case is done. */
template <typename Fn>
static bool gexgl_negative_count(GLsizei n, Fn call)
{
    if (n >= 0) return false;
    call(n, static_cast<GLuint *>(nullptr));
    return true;
}

gexgl_pixel_store pack_store = gexgl_default_pixel_store();
gexgl_pixel_store unpack_store = gexgl_default_pixel_store();
#include "gexgl-compat-host.h"

int get_frontend_param(struct pipe_frontend_screen* fscreen,
                       enum st_manager_param param) {
    (void)fscreen;
    (void)param;
    return 0;
}

/* Which client API the recorded stream was written against.
 *
 * The wire does not carry this: the guest picks it with eglBindAPI, which is
 * a guest-local call, and its default is GLES.  A GLES guest's shader source
 * ("precision mediump float;", attribute/varying, gl_FragColor) does not
 * compile in a core profile, so the backend has to be told which language it
 * is about to be handed.  GLES is the default because it is the transport's
 * own default; gex_gl_init_api selects otherwise for a desktop-GL trace.
 */
int backend_profile = static_cast<int>(API_OPENGLES2);
int backend_major = 3;
int backend_minor = 0;
/* What the guest says its frames belong to, and a count of how many times it
 * has said something different. Read by the page after a submit. */
unsigned attached_kind = 0;
unsigned attached_id = 0;
unsigned attached_token = 0;
unsigned attached_generation = 0;
/* The last batch this device refused, for whoever asks: which opcode, why
 * (1 short read, 2 trailing data, 3 unknown), how big its payload was, and
 * whose it was. */
uint32_t last_reject_op = 0;
uint32_t last_reject_why = 0;
uint32_t last_reject_payload = 0;
uint32_t last_reject_client = 0;
/* Set when a command has already said *why* it refused, so the dispatcher's
 * generic report does not overwrite it with "unhandled opcode" -- which is
 * how a context that could not be created described itself for one confusing
 * afternoon. */
bool last_reject_detailed = false;

/* Everything above that belongs to one *client* rather than to the device.
 *
 * GL object names are the guest's to allocate -- that is the whole point of
 * this protocol -- so two clients hand out the same names, and a single set
 * of these maps gives the second client the first one's objects. Two GL
 * applications at once therefore drew each other's geometry with each other's
 * textures, and closing one broke the other, since the names it deleted were
 * names the survivor was still using.
 *
 * A context each is the fix, and it is what a display server does. The state
 * is swapped rather than referred to through a pointer so that the thousands
 * of lines that use these by name -- including a generated file -- stay as
 * they are; swapping maps is a pointer exchange each, and it happens once per
 * batch at most.
 */
struct client_state {
    std::unordered_map<uint32_t, GLuint> buffers, textures, shaders, programs,
        vertex_arrays, framebuffers, renderbuffers, samplers, queries,
        transform_feedbacks, program_pipelines;
    std::unordered_map<GLuint, std::string> shader_source_heads;
    std::unordered_set<uint32_t> delete_pending_programs;
    uint32_t current_guest_program = 0;
    GLuint default_vao = 0;
    std::vector<GLenum> current_draw_buffers{GL_BACK};
    GLfloat current_clear_color[4] = {0.f, 0.f, 0.f, 0.f};
    GLfloat current_clear_depth = 1.f;
    GLint current_clear_stencil = 0;
    gexgl_pixel_store pack_store = gexgl_default_pixel_store();
    gexgl_pixel_store unpack_store = gexgl_default_pixel_store();
    int backend_profile = static_cast<int>(API_OPENGLES2);
    int backend_major = 3;
    int backend_minor = 0;
    int backend_width = 1;
    int backend_height = 1;
    unsigned attached_kind = 0, attached_id = 0, attached_token = 0;
    struct st_context* mesa_context = nullptr;
    /* A Gallium screen each: see `ensure_screen`. */
    struct pipe_frontend_screen mesa_frontend_screen = {};
#if !defined(GEX_MESA_WGPU)
    struct sw_winsys* mesa_winsys = nullptr;
#endif
    struct st_visual backend_visual = {};
    std::map<uint32_t, gex_surface> surfaces;
    uint32_t current_surface = 0;
};

std::map<uint32_t, client_state> clients;
/* Which client's state is loaded into the globals. Zero is "the only one",
 * which is what a caller that never says who it is gets -- the smoke and
 * conformance harnesses call submit directly. */
uint32_t current_client = 0;

void swap_client_state(client_state& other) {
    std::swap(buffers, other.buffers);
    std::swap(textures, other.textures);
    std::swap(shaders, other.shaders);
    std::swap(programs, other.programs);
    std::swap(vertex_arrays, other.vertex_arrays);
    std::swap(framebuffers, other.framebuffers);
    std::swap(renderbuffers, other.renderbuffers);
    std::swap(samplers, other.samplers);
    std::swap(queries, other.queries);
    std::swap(transform_feedbacks, other.transform_feedbacks);
    std::swap(program_pipelines, other.program_pipelines);
    std::swap(shader_source_heads, other.shader_source_heads);
    std::swap(delete_pending_programs, other.delete_pending_programs);
    std::swap(current_guest_program, other.current_guest_program);
    std::swap(default_vao, other.default_vao);
    std::swap(current_draw_buffers, other.current_draw_buffers);
    for (int i = 0; i < 4; ++i)
        std::swap(current_clear_color[i], other.current_clear_color[i]);
    std::swap(current_clear_depth, other.current_clear_depth);
    std::swap(current_clear_stencil, other.current_clear_stencil);
    std::swap(pack_store, other.pack_store);
    std::swap(unpack_store, other.unpack_store);
    std::swap(backend_profile, other.backend_profile);
    std::swap(backend_major, other.backend_major);
    std::swap(backend_minor, other.backend_minor);
    std::swap(backend_width, other.backend_width);
    std::swap(backend_height, other.backend_height);
    std::swap(attached_kind, other.attached_kind);
    std::swap(attached_id, other.attached_id);
    std::swap(attached_token, other.attached_token);
    std::swap(mesa_context, other.mesa_context);
    std::swap(mesa_frontend_screen, other.mesa_frontend_screen);
#if !defined(GEX_MESA_WGPU)
    std::swap(mesa_winsys, other.mesa_winsys);
#endif
    std::swap(backend_visual, other.backend_visual);
    std::swap(surfaces, other.surfaces);
    std::swap(current_surface, other.current_surface);
}

/* GEX_GL_COUNTS=1 (`?mesaenv=GEX_GL_COUNTS=1`): a line per PRESENT decoded,
 * and one per 256 batches, with the client's batches and presents so far and
 * the surface's frames produced and displayed. For a guest that swaps many
 * times and a window that shows one frame: which of "never arrived", "arrived
 * and was not presented" and "presented and not displayed" it is. */
static bool gl_counts() {
    static int on = -1;
    if (on < 0) {
        const char *value = getenv("GEX_GL_COUNTS");
        on = value && *value && *value != '0';
    }
    return on != 0;
}
static std::map<uint32_t, std::pair<uint64_t, uint64_t>> gl_client_counts;
static void gl_count_line(const char *what) {
    const auto &c = gl_client_counts[current_client];
    fprintf(stderr,
            "gexgl: counts %s -- client %u: %llu batches, %llu presents; "
            "surface %u produced %u displayed %u\n",
            what, current_client, (unsigned long long)c.first,
            (unsigned long long)c.second, sur ? sur->uid : 0u,
            sur ? sur->produced : 0u, sur ? sur->displayed : 0u);
}

/* GEX_GL_ERRORS=1 (`?mesaenv=GEX_GL_ERRORS=1`): glGetError after every
 * command, and each opcode/error pair reported the first few times it
 * happens, with the client. It takes the error the program would have read
 * itself, so it is a diagnostic only. */
static void gl_error_check(uint32_t op) {
    static int on = -1;
    if (on < 0) {
        const char *value = getenv("GEX_GL_ERRORS");
        on = value && *value && *value != '0';
    }
    if (!on) return;
    const GLenum e = glGetError();
    if (e == GL_NO_ERROR) return;
    static std::map<std::pair<uint32_t, uint32_t>, uint32_t> seen;
    const uint32_t n = ++seen[{op, (uint32_t)e}];
    if (n <= 3 || (n & (n - 1)) == 0)
        fprintf(stderr, "gexgl: opcode %u -> GL error 0x%x (%u times, client %u)\n",
                op, (unsigned)e, n, current_client);
}

/* Run what follows as this client. */
void use_client(uint32_t client) {
    if (client == current_client) return;
    const bool fresh = clients.find(client) == clients.end();
    swap_client_state(clients[current_client]);
    current_client = client;
    swap_client_state(clients[client]);
    if (fresh) {
        /* What the page configured for the device, which this client has not
         * had a chance to be told about: it never calls init or set_api --
         * the embedder does, once. */
        backend_profile = device_profile;
        backend_major = device_major;
        backend_minor = device_minor;
        backend_width = device_width;
        backend_height = device_height;
    }
    /* The GL context and the drawable are part of what was just swapped in,
     * so this is where the host's GL is pointed at them. A client whose
     * context does not exist yet gets one from its own CONTEXT_CREATE. */
    sur = nullptr;
    if (mesa_context) {
        gex_surface* into = surface_for(current_surface);
        sur = into;
        st_api_make_current(mesa_context, &into->drawable.base,
                            &into->drawable.base);
        /* And the renderer next door, which keeps a frame per surface: a
         * client switch changes the drawable without going through
         * `use_surface`, so without this the frames of the client that just
         * took over were filed under the window of the one that left. */
        note_surface_to_backend(into->uid);
    }
}

#if defined(GEX_MESA_WGPU)
/* Report which Gallium entry points st/mesa is going to need and this driver
 * does not provide.
 *
 * The corpus in gex_wgpu_smoke.c drives pipe_context directly, so it only ever
 * exercises the hooks it happens to call.  st/mesa calls many more, and a
 * missing one is an indirect call through a null pointer -- "RuntimeError:
 * null function" with no name attached.  Listing them up front turns that into
 * a work list.
 */
int audit_driver_hooks(struct pipe_screen* screen) {
    struct pipe_context* ctx = screen->context_create(screen, nullptr, 0);
    if (!ctx) {
        fprintf(stderr, "gex-wgpu: context_create returned null\n");
        return -1;
    }
    int missing = 0;
#define GEX_NEED(hook)                                                        \
    do {                                                                      \
        if (!ctx->hook) {                                                     \
            fprintf(stderr, "gex-wgpu: missing pipe_context::%s\n", #hook);   \
            ++missing;                                                        \
        }                                                                     \
    } while (0)
    /* Resource access: st/mesa's uploaders map buffers during context
     * creation, before any GL call is decoded. */
    GEX_NEED(buffer_map);
    GEX_NEED(buffer_unmap);
    GEX_NEED(buffer_subdata);
    GEX_NEED(transfer_flush_region);
    GEX_NEED(texture_map);
    GEX_NEED(texture_unmap);
    GEX_NEED(texture_subdata);
    /* Whole-framebuffer clear; clear_render_target alone is not enough. */
    GEX_NEED(clear);
    GEX_NEED(resource_copy_region);
    GEX_NEED(blit);
    GEX_NEED(flush_resource);
    /* Queries: st/mesa creates them for occlusion and timer objects. */
    GEX_NEED(create_query);
    GEX_NEED(destroy_query);
    GEX_NEED(begin_query);
    GEX_NEED(end_query);
    GEX_NEED(get_query_result);
    GEX_NEED(set_active_query_state);
    /* State the frontend sets unconditionally at context init. */
    GEX_NEED(set_sample_mask);
    GEX_NEED(set_min_samples);
    GEX_NEED(set_clip_state);
    GEX_NEED(set_polygon_stipple);
    GEX_NEED(set_debug_callback);
    GEX_NEED(memory_barrier);
    GEX_NEED(texture_barrier);
    GEX_NEED(render_condition);
    /* Teardown paths call these without checking, so a null one turns a
     * refused context version into a crash instead of a fallback. */
    GEX_NEED(sampler_view_release);
    GEX_NEED(sampler_view_destroy);
#undef GEX_NEED
    ctx->destroy(ctx);
    if (missing)
        fprintf(stderr, "gex-wgpu: %d required pipe_context hooks missing\n",
                missing);
    return missing;
}
#endif

/* What the context came up as, and what stands between it and the next
 * desktop OpenGL version.
 *
 * st/mesa derives the version from the extensions it can enable, which come
 * from the driver's caps, so "raise the version" is never a thing to set --
 * it is a list of extensions to earn.  The rungs below are exactly what
 * compute_version() in src/mesa/main/version.c tests, so this list is
 * generated from the authority rather than guessed, and it names the *next*
 * rung only: the one after it is not actionable yet.
 */
void report_version_gap() {
    const GLubyte* version = glGetString(GL_VERSION);
    const GLubyte* glsl = glGetString(GL_SHADING_LANGUAGE_VERSION);
    fprintf(stderr, "gex-wgpu: context is %s / %s\n",
            version ? reinterpret_cast<const char*>(version) : "?",
            glsl ? reinterpret_cast<const char*>(glsl) : "?");

    /* The ladder below is the desktop one, and Mesa numbers an ES context on
     * its own scale -- an ES 2.0 context has Version 20, which the desktop
     * rungs would read as GL 2.0 and report a meaningless gap to GL 2.1. */
    const gl_api api = mesa_context->ctx->API;
    if (api != API_OPENGL_CORE && api != API_OPENGL_COMPAT) {
        fprintf(stderr,
                "gex-wgpu: OpenGL ES context; the desktop ladder does not "
                "apply to it\n");
        return;
    }

    const struct gl_extensions& e = mesa_context->ctx->Extensions;
    const unsigned have = mesa_context->ctx->Version;
    /* Every requirement from GL 4.1 to 4.6 that is not met, not only the next
     * rung's: what stands between this context and 4.6, in one list. The
     * conditions are compute_version()'s (src/mesa/main/version.c). */
    if (have >= 40) {
        const struct gl_constants& c = mesa_context->ctx->Const;
        struct need { unsigned version; const char* name; bool met; };
        const need needs[] = {
            {41, "MaxTextureSize >= 16384", c.MaxTextureSize >= 16384},
            {41, "MaxRenderbufferSize >= 16384", c.MaxRenderbufferSize >= 16384},
            {41, "MaxCubeTextureLevels >= 15", c.MaxCubeTextureLevels >= 15},
            {41, "Max3DTextureLevels >= 12", c.Max3DTextureLevels >= 12},
            {41, "MaxArrayTextureLayers >= 2048", c.MaxArrayTextureLayers >= 2048},
            {41, "ARB_ES2_compatibility", (bool)e.ARB_ES2_compatibility},
            {41, "ARB_shader_precision", (bool)e.ARB_shader_precision},
            {41, "ARB_vertex_attrib_64bit", (bool)e.ARB_vertex_attrib_64bit},
            {41, "ARB_viewport_array", (bool)e.ARB_viewport_array},
            {42, "ARB_base_instance", (bool)e.ARB_base_instance},
            {42, "ARB_conservative_depth", (bool)e.ARB_conservative_depth},
            {42, "ARB_internalformat_query", (bool)e.ARB_internalformat_query},
            {42, "ARB_shader_atomic_counters", (bool)e.ARB_shader_atomic_counters},
            {42, "ARB_shader_image_load_store", (bool)e.ARB_shader_image_load_store},
            {42, "ARB_shading_language_420pack", (bool)e.ARB_shading_language_420pack},
            {42, "ARB_shading_language_packing", (bool)e.ARB_shading_language_packing},
            {42, "ARB_texture_compression_bptc", (bool)e.ARB_texture_compression_bptc},
            {42, "ARB_transform_feedback_instanced", (bool)e.ARB_transform_feedback_instanced},
            {43, "vertex MaxUniformBlocks >= 14", c.Program[MESA_SHADER_VERTEX].MaxUniformBlocks >= 14},
            {43, "ARB_ES3_compatibility", (bool)e.ARB_ES3_compatibility},
            {43, "ARB_arrays_of_arrays", (bool)e.ARB_arrays_of_arrays},
            {43, "ARB_compute_shader", (bool)e.ARB_compute_shader},
            {43, "ARB_copy_image", (bool)e.ARB_copy_image},
            {43, "ARB_explicit_uniform_location", (bool)e.ARB_explicit_uniform_location},
            {43, "ARB_fragment_layer_viewport", (bool)e.ARB_fragment_layer_viewport},
            {43, "ARB_framebuffer_no_attachments", (bool)e.ARB_framebuffer_no_attachments},
            {43, "ARB_internalformat_query2", (bool)e.ARB_internalformat_query2},
            {43, "ARB_robust_buffer_access_behavior", (bool)e.ARB_robust_buffer_access_behavior},
            {43, "ARB_shader_image_size", (bool)e.ARB_shader_image_size},
            {43, "ARB_shader_storage_buffer_object", (bool)e.ARB_shader_storage_buffer_object},
            {43, "ARB_stencil_texturing", (bool)e.ARB_stencil_texturing},
            {43, "ARB_texture_buffer_range", (bool)e.ARB_texture_buffer_range},
            {43, "ARB_texture_query_levels", (bool)e.ARB_texture_query_levels},
            {43, "ARB_texture_view", (bool)e.ARB_texture_view},
            {44, "MaxVertexAttribStride >= 2048", c.MaxVertexAttribStride >= 2048},
            {44, "ARB_buffer_storage", (bool)e.ARB_buffer_storage},
            {44, "ARB_enhanced_layouts", (bool)e.ARB_enhanced_layouts},
            {44, "ARB_query_buffer_object", (bool)e.ARB_query_buffer_object},
            {44, "ARB_texture_mirror_clamp_to_edge", (bool)e.ARB_texture_mirror_clamp_to_edge},
            {44, "ARB_texture_stencil8", (bool)e.ARB_texture_stencil8},
            {44, "ARB_vertex_type_10f_11f_11f_rev", (bool)e.ARB_vertex_type_10f_11f_11f_rev},
            {45, "ARB_ES3_1_compatibility", (bool)e.ARB_ES3_1_compatibility},
            {45, "ARB_clip_control", (bool)e.ARB_clip_control},
            {45, "ARB_conditional_render_inverted", (bool)e.ARB_conditional_render_inverted},
            {45, "ARB_cull_distance", (bool)e.ARB_cull_distance},
            {45, "ARB_derivative_control", (bool)e.ARB_derivative_control},
            {45, "ARB_shader_texture_image_samples", (bool)e.ARB_shader_texture_image_samples},
            {45, "NV_texture_barrier", (bool)e.NV_texture_barrier},
            {46, "ARB_gl_spirv", (bool)e.ARB_gl_spirv},
            {46, "ARB_spirv_extensions", (bool)e.ARB_spirv_extensions},
            {46, "ARB_indirect_parameters", (bool)e.ARB_indirect_parameters},
            {46, "ARB_polygon_offset_clamp", (bool)e.ARB_polygon_offset_clamp},
            {46, "ARB_shader_atomic_counter_ops", (bool)e.ARB_shader_atomic_counter_ops},
            {46, "ARB_shader_draw_parameters", (bool)e.ARB_shader_draw_parameters},
            {46, "ARB_shader_group_vote", (bool)e.ARB_shader_group_vote},
            {46, "ARB_texture_filter_anisotropic", (bool)e.ARB_texture_filter_anisotropic},
            {46, "ARB_transform_feedback_overflow_query", (bool)e.ARB_transform_feedback_overflow_query},
        };
        for (const need& n : needs)
            if (n.version > have && !n.met)
                fprintf(stderr, "gex-wgpu: to 4.6: GL %u.%u wants %s\n",
                        n.version / 10, n.version % 10, n.name);
        fprintf(stderr, "gex-wgpu: to 4.6: constants MaxTextureSize %u "
                "MaxRenderbufferSize %u MaxCubeTextureLevels %u "
                "Max3DTextureLevels %u MaxArrayTextureLayers %u "
                "vertex MaxUniformBlocks %u MaxVertexAttribStride %u "
                "glsl_feature_level %u\n",
                c.MaxTextureSize, c.MaxRenderbufferSize, c.MaxCubeTextureLevels,
                c.Max3DTextureLevels, c.MaxArrayTextureLayers,
                c.Program[MESA_SHADER_VERTEX].MaxUniformBlocks,
                c.MaxVertexAttribStride,
                mesa_frontend_screen.screen->caps.glsl_feature_level);
    }
    /* The driver's advertised level, not ctx->Const.GLSLVersion: Mesa clamps
     * that down to match the GL version it settled on, so reading it back
     * would report the *consequence* of the version as if it were a cause. */
    const unsigned glsl_version =
       mesa_frontend_screen.screen->caps.glsl_feature_level;

    struct rung {
        unsigned version;
        unsigned glsl;
        const char* name;
        /* GLboolean, not bool: gl_extensions stores these as unsigned char and
         * narrowing them inside a braced initializer is ill-formed. */
        GLboolean present;
    };
    const rung ladder[] = {
        {14, 0, "ARB_shadow", e.ARB_shadow},
        {20, 0, "ARB_vertex_shader", e.ARB_vertex_shader},
        {20, 0, "ARB_fragment_shader", e.ARB_fragment_shader},
        {20, 0, "ARB_texture_non_power_of_two", e.ARB_texture_non_power_of_two},
        {20, 0, "EXT_blend_equation_separate", e.EXT_blend_equation_separate},
        {20, 0, "EXT_stencil_two_side", e.EXT_stencil_two_side},
        {21, 0, "EXT_texture_sRGB", e.EXT_texture_sRGB},
        {30, 130, "ARB_color_buffer_float", e.ARB_color_buffer_float},
        {30, 130, "ARB_depth_buffer_float", e.ARB_depth_buffer_float},
        {30, 130, "ARB_half_float_vertex", e.ARB_half_float_vertex},
        {30, 130, "ARB_map_buffer_range", e.ARB_map_buffer_range},
        {30, 130, "ARB_shader_texture_lod", e.ARB_shader_texture_lod},
        {30, 130, "ARB_texture_float", e.ARB_texture_float},
        {30, 130, "ARB_texture_rg", e.ARB_texture_rg},
        {30, 130, "ARB_texture_compression_rgtc", e.ARB_texture_compression_rgtc},
        {30, 130, "EXT_draw_buffers2", e.EXT_draw_buffers2},
        {30, 130, "ARB_framebuffer_object", e.ARB_framebuffer_object},
        {30, 130, "EXT_framebuffer_sRGB", e.EXT_framebuffer_sRGB},
        {30, 130, "EXT_packed_float", e.EXT_packed_float},
        {30, 130, "EXT_texture_array", e.EXT_texture_array},
        {30, 130, "EXT_texture_integer", e.EXT_texture_integer},
        {30, 130, "EXT_texture_shared_exponent", e.EXT_texture_shared_exponent},
        {30, 130, "EXT_transform_feedback", e.EXT_transform_feedback},
        {30, 130, "NV_conditional_render", e.NV_conditional_render},
        {31, 140, "ARB_draw_instanced", e.ARB_draw_instanced},
        {31, 140, "ARB_uniform_buffer_object", e.ARB_uniform_buffer_object},
        {31, 140, "EXT_texture_snorm", e.EXT_texture_snorm},
        {31, 140, "NV_primitive_restart", e.NV_primitive_restart},
        {31, 140, "NV_texture_rectangle", e.NV_texture_rectangle},
        {32, 150, "ARB_depth_clamp", e.ARB_depth_clamp},
        {32, 150, "ARB_draw_elements_base_vertex",
         e.ARB_draw_elements_base_vertex},
        {32, 150, "ARB_fragment_coord_conventions",
         e.ARB_fragment_coord_conventions},
        {32, 150, "EXT_provoking_vertex", e.EXT_provoking_vertex},
        {32, 150, "ARB_seamless_cube_map", e.ARB_seamless_cube_map},
        {32, 150, "ARB_sync", e.ARB_sync},
        {32, 150, "ARB_texture_multisample", e.ARB_texture_multisample},
        {32, 150, "EXT_vertex_array_bgra", e.EXT_vertex_array_bgra},
        {33, 330, "ARB_blend_func_extended", e.ARB_blend_func_extended},
        {33, 330, "ARB_explicit_attrib_location",
         e.ARB_explicit_attrib_location},
        {33, 330, "ARB_instanced_arrays", e.ARB_instanced_arrays},
        {33, 330, "ARB_shader_bit_encoding", e.ARB_shader_bit_encoding},
        {33, 330, "ARB_texture_rgb10_a2ui", e.ARB_texture_rgb10_a2ui},
        {33, 330, "ARB_timer_query", e.ARB_timer_query},
        {33, 330, "ARB_vertex_type_2_10_10_10_rev",
         e.ARB_vertex_type_2_10_10_10_rev},
        {33, 330, "EXT_texture_swizzle", e.EXT_texture_swizzle},
        /* GL 4.0 and above. The ladder stopped at 3.3 once 3.3 was reached,
         * which left the tool answering "at or above the highest rung listed"
         * and naming nothing to do next. Only the extensions Mesa tracks as
         * their own flag are listed: a few of each version's requirements are
         * folded into other flags or always-true in this build, so a rung
         * going quiet here means the tracked ones are done, not that the
         * version is. */
        {40, 400, "ARB_draw_buffers_blend", e.ARB_draw_buffers_blend},
        {40, 400, "ARB_draw_indirect", e.ARB_draw_indirect},
        {40, 400, "ARB_gpu_shader5", e.ARB_gpu_shader5},
        {40, 400, "ARB_gpu_shader_fp64", e.ARB_gpu_shader_fp64},
        {40, 400, "ARB_sample_shading", e.ARB_sample_shading},
        {40, 400, "ARB_tessellation_shader", e.ARB_tessellation_shader},
        {40, 400, "ARB_texture_buffer_object_rgb32",
         e.ARB_texture_buffer_object_rgb32},
        {40, 400, "ARB_texture_cube_map_array", e.ARB_texture_cube_map_array},
        {40, 400, "ARB_texture_gather", e.ARB_texture_gather},
        {40, 400, "ARB_texture_query_lod", e.ARB_texture_query_lod},
        {40, 400, "ARB_transform_feedback2", e.ARB_transform_feedback2},
        {40, 400, "ARB_transform_feedback3", e.ARB_transform_feedback3},
        {41, 410, "ARB_ES2_compatibility", e.ARB_ES2_compatibility},
        {41, 410, "ARB_shader_precision", e.ARB_shader_precision},
        {41, 410, "ARB_vertex_attrib_64bit", e.ARB_vertex_attrib_64bit},
        {41, 410, "ARB_viewport_array", e.ARB_viewport_array},
        {42, 420, "ARB_base_instance", e.ARB_base_instance},
        {42, 420, "ARB_conservative_depth", e.ARB_conservative_depth},
        {42, 420, "ARB_internalformat_query", e.ARB_internalformat_query},
        {42, 420, "ARB_shader_atomic_counters", e.ARB_shader_atomic_counters},
        {42, 420, "ARB_shader_image_load_store",
         e.ARB_shader_image_load_store},
        {42, 420, "ARB_shading_language_420pack",
         e.ARB_shading_language_420pack},
        {42, 420, "ARB_shading_language_packing",
         e.ARB_shading_language_packing},
        {42, 420, "ARB_texture_compression_bptc",
         e.ARB_texture_compression_bptc},
        {42, 420, "ARB_transform_feedback_instanced",
         e.ARB_transform_feedback_instanced},
    };

    /* The next rung above what we have. */
    unsigned next = 0;
    for (const rung& r : ladder) {
        if (r.version > have && (next == 0 || r.version < next))
            next = r.version;
    }
    if (!next) {
        fprintf(stderr, "gex-wgpu: at or above the highest rung listed\n");
        return;
    }

    unsigned missing = 0;
    /* Two rungs are gated by constants rather than extensions, and leaving
     * them out made this report "0 requirements" while the version stayed
     * put. GL 3.0 wants four colour attachments and 4x multisampling --
     * WebGPU has both, so neither is a dead end. */
    if (next == 30) {
        if (mesa_context->ctx->Const.MaxColorAttachments < 4) {
            fprintf(stderr, "gex-wgpu: GL 3.0 wants 4 colour attachments, have %u\n",
                    mesa_context->ctx->Const.MaxColorAttachments);
            ++missing;
        }
        if (mesa_context->ctx->Const.MaxSamples < 4 &&
            !mesa_context->ctx->Const.FakeSWMSAA) {
            fprintf(stderr, "gex-wgpu: GL 3.0 wants 4x multisampling, have %u\n",
                    mesa_context->ctx->Const.MaxSamples);
            ++missing;
        }
    }
    /* ARB_uniform_buffer_object is gated on constants too, and its rung says
     * only the extension's name. st/mesa's can_ubo wants every enabled stage
     * to have twelve uniform blocks -- one fewer than max_const_buffers,
     * since slot zero holds GL's default block -- and to allow indirect
     * constant addressing, plus a 16 KiB uniform block size. */
    if (next == 31) {
        /* Another constant rung, like GL 3.0's colour attachments: the version
         * check wants sixteen texture image units in the vertex stage. */
        const unsigned vertex_units =
            mesa_context->ctx->Const.Program[MESA_SHADER_VERTEX]
                .MaxTextureImageUnits;
        if (vertex_units < 16) {
            fprintf(stderr,
                    "gex-wgpu: GL 3.1 wants 16 vertex texture image units, "
                    "have %u\n", vertex_units);
            ++missing;
        }
        const struct pipe_screen* pscreen = mesa_frontend_screen.screen;
        const mesa_shader_stage ubo_stages[] = {MESA_SHADER_VERTEX,
                                                MESA_SHADER_FRAGMENT};
        for (mesa_shader_stage sh : ubo_stages) {
            fprintf(stderr,
                    "gex-wgpu: %s: MaxUniformBlocks %u (needs 12), "
                    "max_const_buffers %u, indirect_const_addr %d, "
                    "max_instructions %u\n",
                    sh == MESA_SHADER_VERTEX ? "vertex" : "fragment",
                    mesa_context->ctx->Const.Program[sh].MaxUniformBlocks,
                    pscreen->shader_caps[sh].max_const_buffers,
                    (int)pscreen->shader_caps[sh].indirect_const_addr,
                    pscreen->shader_caps[sh].max_instructions);
        }
        fprintf(stderr, "gex-wgpu: MaxUniformBlockSize %u (needs 16384)\n",
                mesa_context->ctx->Const.MaxUniformBlockSize);
    }
    for (const rung& r : ladder) {
        if (r.version != next || r.present)
            continue;
        fprintf(stderr, "gex-wgpu: GL %u.%u wants %s\n",
                next / 10, next % 10, r.name);
        ++missing;
    }
    for (const rung& r : ladder) {
        if (r.version == next && r.glsl && glsl_version < r.glsl) {
            fprintf(stderr, "gex-wgpu: GL %u.%u wants GLSL %u, have %u\n",
                    next / 10, next % 10, r.glsl, glsl_version);
            ++missing;
            break;
        }
    }
    fprintf(stderr, "gex-wgpu: %u requirement(s) between GL %u.%u and %u.%u\n",
            missing, have / 10, have % 10, next / 10, next % 10);
}

/* The depth/stencil format the driver under us will actually take.
 *
 * Which byte order of Z24S8 a driver has is a driver's business, so ask
 * rather than assume; the order here is preference.
 */
static enum pipe_format depth_stencil_for(struct pipe_screen *screen)
{
    static const enum pipe_format wanted[] = {
        PIPE_FORMAT_Z24_UNORM_S8_UINT,
        PIPE_FORMAT_S8_UINT_Z24_UNORM,
        PIPE_FORMAT_Z32_FLOAT_S8X24_UINT,
    };
    if (screen) {
        for (unsigned i = 0; i < sizeof wanted / sizeof *wanted; i++) {
            if (screen->is_format_supported(screen, wanted[i], PIPE_TEXTURE_2D, 1, 1,
                                            PIPE_BIND_DEPTH_STENCIL))
                return wanted[i];
        }
    }
    return PIPE_FORMAT_Z24_UNORM_S8_UINT;
}

/* This client's Gallium screen, made if it has none yet.
 *
 * One screen per client, not one per device: the screen is where a driver
 * keeps its per-client caches.
 */
int ensure_screen() {
    if (mesa_frontend_screen.screen) return 0;
#if defined(GEX_MESA_WGPU)
    /* The page owns the device; the screen only retains its queue. */
    WGPUDevice device = emscripten_webgpu_get_device();
    if (!device) return -1;
    mesa_frontend_screen.screen = gex_wgpu_screen_create(device);
#else
    mesa_winsys = null_sw_create();
    if (!mesa_winsys) return -1;

    mesa_frontend_screen.screen = softpipe_create_screen(mesa_winsys);
#endif
    mesa_frontend_screen.get_param = get_frontend_param;
    if (!mesa_frontend_screen.screen) return -2;
#if defined(GEX_MESA_WGPU)
    audit_driver_hooks(mesa_frontend_screen.screen);
#endif
    return 0;
}

/* What the page asks for once, before any client exists: the size everything
 * starts at, and a screen for whoever is current -- which also reports at
 * once whether this backend works at all. The context itself waits for
 * CONTEXT_CREATE, where the guest finally knows whether the application
 * wants desktop GL or ES. */
int initialize_backend(int width, int height) {
    backend_width = width;
    backend_height = height;
    /* And for clients that turn up later: see `device_width`. */
    device_width = width;
    device_height = height;
    return ensure_screen();
}

/* Create the GL context the guest asked for.
 *
 * `api` is 0 for OpenGL ES and 1 for desktop OpenGL, as
 * crates/gex-gl/src/proto.rs defines it.  A zero major means "the best you
 * can do", which is what an application that never named a version should
 * get: st/mesa grants the highest version its extension set supports.
 */
int create_backend_context(unsigned api, unsigned major, unsigned minor,
                           unsigned profile) {
    if (mesa_context)
        return 0;
    /* This client's own screen: the page only ever made one, for whoever
     * was current at the time. */
    if (ensure_screen() != 0 || !mesa_frontend_screen.screen)
        return -1;

    /* A visual the guest's default framebuffer can actually be: a colour
     * buffer it draws to and a depth/stencil buffer, both at the size the
     * transport was given. */
    struct st_visual visual = {};
    visual.buffer_mask = ST_ATTACHMENT_FRONT_LEFT_MASK |
                         ST_ATTACHMENT_BACK_LEFT_MASK |
                         ST_ATTACHMENT_DEPTH_STENCIL_MASK;
    visual.color_format = PIPE_FORMAT_R8G8B8A8_UNORM;
    visual.depth_stencil_format = depth_stencil_for(mesa_frontend_screen.screen);
    visual.accum_format = PIPE_FORMAT_NONE;
    visual.samples = 0;

    /* st/mesa computes the version it can actually support while creating the
     * context, and then only *compares* it with what was asked for: a request
     * of 1.0 skips that comparison entirely.  So ask for 1.0, read back the
     * version it settled on, and enforce the application's request here.  That
     * is one context instead of a descending ladder of create-and-destroy, and
     * the number it yields is the real ceiling rather than the best of a list
     * somebody has to keep up to date.
     *
     * A core profile only exists from 3.2 up; below that the compatibility
     * profile is the one that is defined.
     */
    struct st_context_attribs attribs = {};
    attribs.visual = visual;
    attribs.major = 1;
    attribs.minor = 0;
    /* `profile` is 1 when the guest requires compatibility -- a GLX context
     * created the legacy way. The difference that matters here is the default
     * vertex array object, which core does not have and GL2-era code assumes:
     * without it st drops every draw before the driver is called. */
    attribs.profile = api != 1 ? API_OPENGLES2
                    : profile == 1 || major * 10 + minor < 32 ? API_OPENGL_COMPAT : API_OPENGL_CORE;

    fprintf(stderr, "gex-wgpu: context requested api=%u %u.%u\n",
            api, major, minor);
    enum st_context_error error = ST_CONTEXT_SUCCESS;
    mesa_context = st_api_create_context(&mesa_frontend_screen, &attribs,
                                         &error, nullptr);
    if (api == 1 && attribs.profile == API_OPENGL_CORE &&
        (!mesa_context || mesa_context->ctx->Version < 32)) {
        /* Either the core profile could not be created at all, or it came up
         * below 3.2 where a core profile is not defined. The compatibility
         * profile is what a driver of this capability actually offers, so try
         * it before giving up -- an application asking for desktop GL should
         * get the desktop context that exists, not none. */
        if (mesa_context) {
            st_destroy_context(mesa_context);
            mesa_context = nullptr;
        }
        attribs.profile = API_OPENGL_COMPAT;
        mesa_context = st_api_create_context(&mesa_frontend_screen, &attribs,
                                             &error, nullptr);
    }
    if (!mesa_context) {
        fprintf(stderr,
                "gex-wgpu: no %s context: st error %d\n",
                api == 1 ? "desktop GL" : "GLES", (int)error);
        return -3 - static_cast<int>(error);
    }
    fprintf(stderr, "gex-wgpu: negotiated %s %u.%u\n",
            api == 1 ? "desktop GL" : "GLES",
            mesa_context->ctx->Version / 10, mesa_context->ctx->Version % 10);

    /* The application named a version this context cannot reach. Say so and
     * hand back the context that does exist.
     *
     * Destroying it instead was worse in a way that took a real application to
     * see. The guest's context_create has no failure path -- eglCreateContext
     * has already returned by the time this runs -- so the refusal could only
     * surface as CONTEXT_CREATE failing, which the decoder reports as a
     * protocol error, which latches the transport. SuperTuxKart asks for 4.4,
     * and the entire session died on the request rather than on any use of it.
     *
     * What makes granting it defensible is that nothing is misrepresented:
     * glGetString reports this context's real version, which the guest passes
     * through verbatim, so an application that checks gets the truth and one
     * that calls a 4.4 entry point anyway gets an ordinary GL error for that
     * call. Refusing up front is only better when the refusal can be reported,
     * and here it cannot. */
    if (major && mesa_context->ctx->Version < major * 10u + minor) {
        fprintf(stderr,
                "gex-wgpu: application asked for %u.%u, this context is %u.%u "
                "(granted; GL_VERSION reports the real one)\n",
                major, minor, mesa_context->ctx->Version / 10,
                mesa_context->ctx->Version % 10);
    }

    /* Every drawable of this client is built with this visual, including the
     * ones that turn up later: a window created after the context is a
     * surface of its own now. */
    backend_visual = visual;
    /* And the ones that turned up *before* it. The two commands arrive in
     * either order -- a GLX program attaches its window inside
     * glXMakeCurrent, just before the context is made -- so a surface can
     * already exist with no visual at all, and a context made current
     * against a drawable with no colour format simply fails. That is what
     * -9 was: a window attached first, and glxgears with no context. */
    for (auto& entry : surfaces) {
        gex_drawable& drawable = entry.second.drawable;
        if (drawable.visual.color_format != PIPE_FORMAT_NONE) continue;
        drawable.visual = visual;
        drawable.width = backend_width > 0 ? (unsigned)backend_width : 1;
        drawable.height = backend_height > 0 ? (unsigned)backend_height : 1;
        ++drawable.base.stamp;
    }

    gex_surface* into = surface_for(current_surface);
    sur = into;
    if (!st_api_make_current(mesa_context, &into->drawable.base,
                             &into->drawable.base))
        return -9;
    note_surface_to_backend(into->uid);
#if defined(GEX_MESA_WGPU)
    report_version_gap();
#endif
    return 0;
}

#if defined(GEX_MESA_WGPU)
/* Putting the drawable on the canvas.
 *
 * st/mesa's drawable is a plain texture this file owns -- nothing is scanned
 * out, which is what makes replay deterministic -- so presenting is a copy
 * from that texture into the canvas surface's current texture. The surface is
 * configured RGBA8Unorm to match the visual's colour format exactly: WebGPU
 * will not copy between rgba8unorm and bgra8unorm, and a blit through a
 * render pass to swap two channels would be a shader and a pipeline for
 * something the configuration can simply avoid.
 *
 * Without this the whole path renders correctly into a texture nobody ever
 * sees: every frame counted as presented over a canvas that stayed black. */
WGPUSurface mesa_surface = nullptr;

/* One-shot markers through the first present.
 *
 * The WebGPU device was destroyed 1.5 ms after the first frame was presented,
 * with no validation error and nothing in JavaScript calling `destroy()`. The
 * only way to tell which of these calls did it is to say where we are before
 * each one, once. */
#define GEX_PRESENT_MARK(what)                                                \
    do {                                                                      \
        static bool said_##what;                                              \
        if (!said_##what) {                                                   \
            said_##what = true;                                               \
            fprintf(stderr, "gex-wgpu: first present -- " #what "\n");        \
        }                                                                     \
    } while (0)

/* Declared here rather than with the other externs at the top: WGPUTexture
 * only exists once webgpu.h has been included. */
extern "C" WGPUTexture gex_wgpu_presented_texture;

/* Resize the default framebuffer to what the application asked for.
 *
 * This used to read its two arguments and throw them away, so the default
 * framebuffer was always the canvas's size no matter what
 * eglCreatePbufferSurface or eglCreateWindowSurface requested -- while
 * eglQuerySurface, answered on the guest side, reported the requested size
 * faithfully. Everything that draws to framebuffer 0 and reads it back at the
 * size it asked for was therefore wrong: the CTS asks for 64x64, believed it
 * had 64x64, drew across 320x240 and checked pixels in the corner.
 * KHR-GL33.primitive_restart failed on exactly that, with primitive restart
 * itself working perfectly.
 *
 * Three things have to move together. The cached attachments are the old
 * size, so they are dropped and drawable_validate builds them again; st only
 * asks for them when the drawable's stamp changes, so that is bumped; and the
 * canvas surface is configured for the old size, so it is released and
 * configure_present_surface redoes it. */
void resize_drawable(int width, int height) {
    if (width <= 0 || height <= 0) return;
    /* Also the size a *later* context creation will build its drawable at.
     *
     * The two commands can arrive in either order. A GLX program resizes
     * through glXMakeCurrent -- eglCreateWindowSurface reads the window's
     * geometry -- and the context is created by the same call, just after; so
     * the resize landed first and context_create then rebuilt mesa_drawable
     * from these, throwing it away. glmark2 asked for 800x600, got 300x300,
     * and rendered its whole scene outside the framebuffer. */
    backend_width = width;
    backend_height = height;
    if ((unsigned)width == sur->drawable.width &&
        (unsigned)height == sur->drawable.height)
        return;
    sur->drawable.width = (unsigned)width;
    sur->drawable.height = (unsigned)height;
    for (struct pipe_resource*& attachment : sur->drawable.attachments)
        pipe_resource_reference(&attachment, nullptr);
    ++sur->drawable.base.stamp;
    /* Counted-draw bookkeeping only, but it names a texture that is gone. */
    gex_wgpu_presented_texture = nullptr;
    if (mesa_surface) {
        wgpuSurfaceRelease(mesa_surface);
        mesa_surface = nullptr;
    }
}

/* The device's queue, fetched once and kept.
 *
 * This used to be fetched and released around every present. `webgpu.h` says
 * the getter hands out a reference the caller owns, so releasing it is
 * correct by the letter -- and with this implementation the device stopped
 * existing a frame or two later, reported as
 * `lost: destroyed: Device was destroyed`, which is a device that has quietly
 * stopped doing anything: no failed calls, no validation errors, frame
 * counters still climbing, and a black window. Holding the queue for the
 * device's whole life also saves a fetch per frame, which is what a driver
 * would do anyway. */
WGPUQueue present_queue(WGPUDevice device) {
    static WGPUQueue queue = nullptr;
    if (!queue) queue = wgpuDeviceGetQueue(device);
    return queue;
}

/* The device frames are shown with: this client's screen's, or for a
 * client with no GL of its own (one whose frames come from the D3D device,
 * gexd3d-mesa.cpp) the page's, which every screen is made on. */
WGPUDevice display_device() {
    return mesa_frontend_screen.screen
        ? gex_wgpu_screen_device(mesa_frontend_screen.screen)
        : emscripten_webgpu_get_device();
}

void configure_present_surface() {
    if (mesa_surface) return;
    GEX_PRESENT_MARK(configure_begin);
    WGPUDevice device = display_device();
    if (!device) return;
    WGPUInstance instance = wgpuCreateInstance(nullptr);
    if (!instance) return;
    WGPUEmscriptenSurfaceSourceCanvasHTMLSelector source{};
    source.chain.sType = WGPUSType_EmscriptenSurfaceSourceCanvasHTMLSelector;
    source.selector = WGPUStringView{"#gex-gl", WGPU_STRLEN};
    WGPUSurfaceDescriptor descriptor{};
    descriptor.nextInChain = &source.chain;
    GEX_PRESENT_MARK(create_surface);
    mesa_surface = wgpuInstanceCreateSurface(instance, &descriptor);
    if (!mesa_surface) {
        fprintf(stderr, "gex-wgpu: no canvas surface; nothing will be shown\n");
        return;
    }
    fprintf(stderr, "gex-wgpu: first present -- canvas resize to %ux%u\n",
            sur->drawable.width, sur->drawable.height);
    emscripten_set_canvas_element_size("#gex-gl", (int)sur->drawable.width,
                                       (int)sur->drawable.height);
    WGPUSurfaceConfiguration cfg{};
    cfg.device = device;
    cfg.format = WGPUTextureFormat_RGBA8Unorm;
    /* Read as well as written. The frame is presented into this canvas and
     * then *taken* from it: the compositor with a scene uploads the canvas
     * into a texture, and the one that puts each window in an element
     * copies it into the window's own canvas. Both are ordinary image-source
     * reads of a canvas, and a WebGPU canvas configured without CopySrc
     * cannot be one -- which did not fail the read so much as take the device
     * with it: `lost: destroyed` in the same breath as the first present, and
     * a black window from then on, with draws still counting up. */
    cfg.usage = WGPUTextureUsage_RenderAttachment | WGPUTextureUsage_CopyDst |
                WGPUTextureUsage_CopySrc | WGPUTextureUsage_TextureBinding;
    cfg.width = sur->drawable.width;
    cfg.height = sur->drawable.height;
    cfg.alphaMode = WGPUCompositeAlphaMode_Opaque;
    cfg.presentMode = WGPUPresentMode_Fifo;
    GEX_PRESENT_MARK(surface_configure);
    wgpuSurfaceConfigure(mesa_surface, &cfg);
    GEX_PRESENT_MARK(configure_done);
}

/* Nothing keeps a frame per surface on this backend: it presents into a
 * canvas, the window's own or the shared one. */
void note_surface_to_backend(unsigned uid) { (void)uid; }

/* A canvas of the window's own, by surface uid.
 *
 * The shared `#gex-gl` canvas holds one frame, so the page used to copy it
 * into each window's 2D canvas with `drawImage`. That read of a WebGPU
 * canvas through 2D came back fully transparent on Chromium on Linux
 * -- a red clear included -- so a card game counted every frame
 * presented and showed none. A page that can give the window a canvas
 * names it `#gex-gl-<uid>` (`gex_gl_surface_canvas`), and the frame is
 * copied there texture to texture: on the GPU all the way, nothing read
 * back, and one frame per window rather than whichever presented last.
 *
 * `reshow` is a frame owed to a canvas that arrived after the frame did:
 * a client that draws only when something changes (a card game) would
 * otherwise leave its new canvas empty until the next click. */
struct window_canvas {
    WGPUSurface surface = nullptr;
    unsigned width = 0, height = 0;
    bool reshow = false;
};
std::map<unsigned, window_canvas> window_canvases;

void forget_window_canvas(unsigned uid) {
    auto found = window_canvases.find(uid);
    if (found == window_canvases.end()) return;
    if (found->second.surface) wgpuSurfaceRelease(found->second.surface);
    window_canvases.erase(found);
}

/* The surface to present the current window into, configured at its size:
 * its own canvas when the page gave it one, the shared canvas otherwise. */
WGPUSurface present_target(WGPUDevice device) {
    auto found = window_canvases.find(sur->uid);
    if (found == window_canvases.end()) {
        configure_present_surface();
        return mesa_surface;
    }
    window_canvas& wc = found->second;
    const unsigned w = sur->drawable.width, h = sur->drawable.height;
    if (wc.surface && wc.width == w && wc.height == h) return wc.surface;
    char selector[32];
    snprintf(selector, sizeof selector, "#gex-gl-%u", sur->uid);
    if (!wc.surface) {
        static WGPUInstance instance = wgpuCreateInstance(nullptr);
        if (!instance) return nullptr;
        WGPUEmscriptenSurfaceSourceCanvasHTMLSelector source{};
        source.chain.sType = WGPUSType_EmscriptenSurfaceSourceCanvasHTMLSelector;
        source.selector = WGPUStringView{selector, WGPU_STRLEN};
        WGPUSurfaceDescriptor descriptor{};
        descriptor.nextInChain = &source.chain;
        wc.surface = wgpuInstanceCreateSurface(instance, &descriptor);
        if (!wc.surface) {
            static bool said;
            if (!said) {
                said = true;
                fprintf(stderr, "gex-wgpu: no surface for %s; that window "
                        "falls back to the shared canvas\n", selector);
            }
            window_canvases.erase(found);
            configure_present_surface();
            return mesa_surface;
        }
    }
    emscripten_set_canvas_element_size(selector, (int)w, (int)h);
    WGPUSurfaceConfiguration cfg{};
    cfg.device = device;
    cfg.format = WGPUTextureFormat_RGBA8Unorm;
    /* Written only: nothing reads this canvas back. */
    cfg.usage = WGPUTextureUsage_RenderAttachment | WGPUTextureUsage_CopyDst;
    cfg.width = w;
    cfg.height = h;
    cfg.alphaMode = WGPUCompositeAlphaMode_Opaque;
    cfg.presentMode = WGPUPresentMode_Fifo;
    wgpuSurfaceConfigure(wc.surface, &cfg);
    wc.width = w;
    wc.height = h;
    return wc.surface;
}

void present_backend() {
    glFinish();
    ++produced;
    if (sur) {
        ++sur->produced;
        pipe_resource_reference(&sur->shown,
                                sur->drawable.attachments[ST_ATTACHMENT_BACK_LEFT]);
    }
}

/* True when this surface has something to show: a new frame, or its last one
 * owed to a canvas that arrived after it. */
bool display_pending(const gex_surface& s) {
    if (s.displayed < s.produced) return true;
    if (!s.produced) return false;
    auto found = window_canvases.find(s.uid);
    return found != window_canvases.end() && found->second.reshow;
}

/* The current surface's newest frame has been shown. A GL client is paced
 * by these counts, one frame each; a D3D window is not, and its frames are
 * shown newest-first: the ones it presented in between are passed over, not
 * shown later one per page frame as though each were new. */
void note_shown() {
    count_shown();
    if (sur->external) {
        sur->displayed = sur->produced;
        ++displayed_d3d;
        d3d_behind = sur->produced - sur->displayed;
    } else {
        ++sur->displayed;
        ++displayed_gl;
    }
}

bool display_backend() {
    if (!sur || !display_pending(*sur)) return false;
    /* A reshow repeats a frame already counted: it moves no counter. */
    const bool fresh = sur->displayed < sur->produced;
    {
        auto found = window_canvases.find(sur->uid);
        if (found != window_canvases.end()) found->second.reshow = false;
    }
    /* The presented frame's buffer, which a resize since has not taken; a
     * D3D window's frames are its back buffer itself. */
    struct pipe_resource *source = sur->shown && !sur->external
        ? sur->shown : sur->drawable.attachments[ST_ATTACHMENT_BACK_LEFT];
    WGPUTexture from = source ? gex_wgpu_resource_texture(source) : nullptr;
    WGPUDevice device = display_device();
    WGPUSurface target = device ? present_target(device) : nullptr;
    if (!target || !from || !device) {
        /* Counted anyway: the frame really was produced, and reporting
         * otherwise would stall a caller that waits for the count to move.
         * Said out loud once, because counting a frame nobody copied is
         * indistinguishable from presenting one -- the caller sees a rising
         * `presented` and a canvas that never changes. */
        static bool said;
        if (!said) {
            said = true;
            fprintf(stderr,
                    "gex-wgpu: display copies nothing (%s); the canvas will "
                    "not change while frames keep counting\n",
                    !target ? "no canvas surface"
                        : !from ? "no back-left drawable texture"
                                : "no device");
        }
        if (fresh) note_shown();
        return true;
    }
    /* Tell the driver which texture reaches the canvas, so it can count the
     * draws that land in it rather than in some other same-sized target. */
    gex_wgpu_presented_texture = from;
    WGPUSurfaceTexture surface_texture{};
    GEX_PRESENT_MARK(get_current_texture);
    wgpuSurfaceGetCurrentTexture(target, &surface_texture);
    if (surface_texture.status !=
            WGPUSurfaceGetCurrentTextureStatus_SuccessOptimal &&
        surface_texture.status !=
            WGPUSurfaceGetCurrentTextureStatus_SuccessSuboptimal) {
        static bool said;
        if (!said) {
            said = true;
            fprintf(stderr,
                    "gex-wgpu: surface texture unavailable (status %u); the "
                    "canvas will not change while frames keep counting\n",
                    (unsigned)surface_texture.status);
        }
        if (fresh) note_shown();
        return true;
    }
    {
        /* Every ~10 s of frames: how many draws actually reached the device
         * against how many were declined. A frame presents either way, so this
         * is what distinguishes "drawing nothing" from "not started". */
        static unsigned frames;
        /* GEX_WGPU_FRAME_REPORT=N (`?mesaenv=GEX_WGPU_FRAME_REPORT=1`): the
         * report every N frames instead, for a program that presents a
         * handful of frames and whose second one is the question (which
         * texture its draws and blits went to, against the one shown). */
        static unsigned every;
        if (!every) {
            const char *value = getenv("GEX_WGPU_FRAME_REPORT");
            every = value && atoi(value) > 0 ? (unsigned)atoi(value) : 600;
        }
        /* Braced: the report below was once outside the `if` by mistake, so
         * its ten formatted stderr lines (and the page's console plumbing
         * for each) ran on every displayed frame. */
        if ((frames++ % every) == 0) {
            fprintf(stderr,
                    "gex-wgpu: frame %u -- issued %u, declined %u "
                    "(constant %u, texture %u, vertex %u; last: %s); "
                    "new frames shown: %u GL, %u D3D (%u behind)\n",
                    frames, gex_wgpu_draws_issued, gex_wgpu_draws_declined,
                    gex_wgpu_declined_constant, gex_wgpu_declined_texture,
                    gex_wgpu_declined_vertex,
                    gex_wgpu_declined_last_why ? gex_wgpu_declined_last_why
                                               : "-",
                    displayed_gl, displayed_d3d, d3d_behind);
            gex_wgpu_report_draw_targets();
        }
    }
    GEX_PRESENT_MARK(copy_begin);
    WGPUCommandEncoder encoder = wgpuDeviceCreateCommandEncoder(device, nullptr);
    if (encoder) {
        WGPUTexelCopyTextureInfo src = WGPU_TEXEL_COPY_TEXTURE_INFO_INIT;
        src.texture = from;
        WGPUTexelCopyTextureInfo dst = WGPU_TEXEL_COPY_TEXTURE_INFO_INIT;
        dst.texture = surface_texture.texture;
        /* The smallest of the three: a window canvas is sized to its
         * drawable, the shared one may still be at its old size for a frame,
         * and the frame shown may be from before a resize. */
        WGPUExtent3D size{
            std::min({sur->drawable.width,
                      wgpuTextureGetWidth(surface_texture.texture),
                      wgpuTextureGetWidth(from)}),
            std::min({sur->drawable.height,
                      wgpuTextureGetHeight(surface_texture.texture),
                      wgpuTextureGetHeight(from)}),
            1};
        wgpuCommandEncoderCopyTextureToTexture(encoder, &src, &dst, &size);
        WGPUCommandBuffer commands = wgpuCommandEncoderFinish(encoder, nullptr);
        if (commands) {
            GEX_PRESENT_MARK(submit);
            wgpuQueueSubmit(present_queue(device), 1, &commands);
            wgpuCommandBufferRelease(commands);
        }
        wgpuCommandEncoderRelease(encoder);
    }
    GEX_PRESENT_MARK(texture_release);
    wgpuTextureRelease(surface_texture.texture);
    GEX_PRESENT_MARK(done);
    if (fresh) note_shown();
    return true;
}

/* The page has given surface `uid` a canvas of its own, `#gex-gl-<uid>`, or
 * (`on` zero) taken it away. Its last frame is shown there at the next
 * display, without counting as a new one. */
void set_window_canvas(unsigned uid, bool on) {
    if (!on) {
        forget_window_canvas(uid);
        return;
    }
    window_canvas& wc = window_canvases[uid];
    /* A canvas named again may be a new element under the old id: build its
     * surface afresh. */
    if (wc.surface) {
        wgpuSurfaceRelease(wc.surface);
        wc.surface = nullptr;
    }
    wc.width = wc.height = 0;
    wc.reshow = true;
}
#else
void note_surface_to_backend(unsigned uid) { (void)uid; }
void forget_window_canvas(unsigned uid) { (void)uid; }
void set_window_canvas(unsigned uid, bool on) { (void)uid; (void)on; }
bool display_pending(const gex_surface& s) { return s.displayed < s.produced; }

void present_backend() {
    glFinish();
    ++produced;
    if (sur) {
        ++sur->produced;
        pipe_resource_reference(&sur->shown,
                                sur->drawable.attachments[ST_ATTACHMENT_BACK_LEFT]);
    }
}

bool display_backend() {
    if (!sur || sur->displayed >= sur->produced) return false;
    ++sur->displayed;
    count_shown();
    return true;
}

/* Give the current drawable the size the guest's window has.
 *
 * This used to do nothing at all, with a comment about there being no
 * drawable of ours -- true of the backend that draws into an embedder's
 * surface, and not of this one, which owns its drawable. So a window's size
 * never reached it: the drawable stayed whatever the transport was started
 * with, a 300x300 window drew into a 300x150 framebuffer, and the top half of
 * the gears was simply missing. The attachments are dropped so
 * drawable_validate builds them again, and the stamp is bumped because that
 * is the only thing st/mesa re-reads them for. */
void resize_drawable(int width, int height) {
    if (width <= 0 || height <= 0) return;
    backend_width = width;
    backend_height = height;
    if (!sur) return;
    if ((unsigned)width == sur->drawable.width &&
        (unsigned)height == sur->drawable.height)
        return;
    sur->drawable.width = (unsigned)width;
    sur->drawable.height = (unsigned)height;
    for (struct pipe_resource*& attachment : sur->drawable.attachments)
        pipe_resource_reference(&attachment, nullptr);
    ++sur->drawable.base.stamp;
}
#endif

void put_u32(uint32_t v) {
    const size_t at = reply.size(); reply.resize(at + 4); std::memcpy(reply.data() + at, &v, 4);
}
void put_u64(uint64_t v) {
    put_u32(static_cast<uint32_t>(v));
    put_u32(static_cast<uint32_t>(v >> 32));
}
void put_blob(const void* p, uint32_t n) {
    const size_t padded = (static_cast<size_t>(n) + 3u) & ~size_t{3};
    put_u32(n); const size_t at = reply.size(); reply.resize(at + padded, 0);
    if (p && n) std::memcpy(reply.data() + at, p, n);
}
GLuint lookup(const std::unordered_map<uint32_t, GLuint>& map, uint32_t name) {
    if (!name) return 0; auto it = map.find(name); return it == map.end() ? 0 : it->second;
}

/* Binding a name the application never generated is GL_INVALID_OPERATION in a
   core profile, and mapping the unknown name to zero turned that into a
   silent unbind -- KHR-GL32.buffer_objects.gen_buffers checks exactly this.
   The guest allocates its own names, so the host cannot be handed the same
   one; a name the host's GL has itself never generated provokes the same
   error, which is the answer GL owes the application. */
constexpr GLuint GEXGL_UNGENERATED_NAME = 0x7ffffffeu;

GLuint lookup_bindable(const std::unordered_map<uint32_t, GLuint>& map,
                       uint32_t name) {
    if (!name) return 0;
    auto it = map.find(name);
    return it == map.end() ? GEXGL_UNGENERATED_NAME : it->second;
}

/* Programs and shaders share one name space in GL, and the difference between
   the two errors a program-typed query owes is which table the name is in:
   GL_INVALID_OPERATION for a shader, GL_INVALID_VALUE for a name GL never
   generated. Looking a shader up in the programs table alone produced zero,
   which the host then reported as GL_INVALID_VALUE -- the wrong one, and the
   only thing KHR-GL32.get_uniform_tests was still failing on. Searching both
   hands the host the real object and lets it answer for itself. */
GLuint lookup_program_or_shader(uint32_t name) {
    if (!name) return 0;
    auto program = programs.find(name);
    if (program != programs.end()) return program->second;
    auto shader = shaders.find(name);
    if (shader != shaders.end()) return shader->second;
    return GEXGL_UNGENERATED_NAME;
}
uint32_t external_name(const std::unordered_map<uint32_t, GLuint>& map, GLuint name) {
    if (!name) return 0;
    for (const auto& [external, internal] : map) {
        if (internal == name) return external;
    }
    return 0;
}

template <typename DeleteObjects>
void delete_guest_objects(std::unordered_map<uint32_t, GLuint>& map, DeleteObjects deleteObjects) {
    // A corrupted/old transport could already contain two guest aliases for one
    // host object.  Deduplicate before deleting so recovery itself is safe.
    std::unordered_set<GLuint> unique;
    for (const auto& [guest, host] : map) {
        (void)guest;
        if (host) unique.insert(host);
    }
    std::vector<GLuint> names(unique.begin(), unique.end());
    if (!names.empty()) deleteObjects(static_cast<GLsizei>(names.size()), names.data());
    map.clear();
}

void reset_guest_namespace() {

    glUseProgram(0);
    current_guest_program = 0;
    delete_pending_programs.clear();

    for (const auto& [guest, host] : programs) {
        (void)guest;
        if (host) glDeleteProgram(host);
    }
    programs.clear();
    for (const auto& [guest, host] : shaders) {
        (void)guest;
        if (host) glDeleteShader(host);
    }
    shaders.clear();

    delete_guest_objects(buffers, glDeleteBuffers);
    delete_guest_objects(queries, glDeleteQueries);
    delete_guest_objects(transform_feedbacks, glDeleteTransformFeedbacks);
    delete_guest_objects(program_pipelines, [](GLsizei count, const GLuint* names) {
        mesa_proc<decltype(&glDeleteProgramPipelines)>("glDeleteProgramPipelines")(count, names);
    });
    delete_guest_objects(textures, glDeleteTextures);
    delete_guest_objects(samplers, glDeleteSamplers);
    delete_guest_objects(vertex_arrays, glDeleteVertexArrays);
    delete_guest_objects(framebuffers, glDeleteFramebuffers);
    delete_guest_objects(renderbuffers, glDeleteRenderbuffers);

    current_draw_buffers.assign(1, GL_BACK);
}
size_t integer_query_components(GLenum pname) {
    switch (pname) {
    case GL_VIEWPORT:
    case GL_SCISSOR_BOX:
    case GL_COLOR_WRITEMASK:
    case GL_COLOR_CLEAR_VALUE:
    case GL_BLEND_COLOR:
        return 4;
    case GL_DEPTH_RANGE:
    case GL_MAX_VIEWPORT_DIMS:
    case GL_ALIASED_POINT_SIZE_RANGE:
    case GL_ALIASED_LINE_WIDTH_RANGE:
    case GL_POINT_SIZE_RANGE:
    case GL_SMOOTH_LINE_WIDTH_RANGE:
    case GL_VIEWPORT_BOUNDS_RANGE:
    case GL_POLYGON_MODE:
        return 2;
    case GL_MAX_COMPUTE_WORK_GROUP_COUNT:
    case GL_MAX_COMPUTE_WORK_GROUP_SIZE:
        return 3;
    case GL_COMPRESSED_TEXTURE_FORMATS: {
        GLint count = 0;
        glGetIntegerv(GL_NUM_COMPRESSED_TEXTURE_FORMATS, &count);
        return static_cast<size_t>(std::clamp(count, 0, 4096));
    }
    case GL_PROGRAM_BINARY_FORMATS: {
        GLint count = 0;
        glGetIntegerv(GL_NUM_PROGRAM_BINARY_FORMATS, &count);
        return static_cast<size_t>(std::clamp(count, 0, 4096));
    }
    case GL_SHADER_BINARY_FORMATS: {
        GLint count = 0;
        glGetIntegerv(GL_NUM_SHADER_BINARY_FORMATS, &count);
        return static_cast<size_t>(std::clamp(count, 0, 4096));
    }
    default:
        /* The fixed-function state, matrices among them, and every other
         * multi-valued pname the shared table knows. */
        return gexgl_pname_count(pname);
    }
}

uint32_t gexgl_state_query_components(GLenum pname) {
    return static_cast<uint32_t>(integer_query_components(pname));
}

uint32_t uniform_components(GLuint program, GLint location) {
    GLint count = 0, max_name = 0;
    glGetProgramiv(program, GL_ACTIVE_UNIFORMS, &count);
    glGetProgramiv(program, GL_ACTIVE_UNIFORM_MAX_LENGTH, &max_name);
    std::vector<char> name(std::max(max_name, 1));
    GLenum type = 0;
    for (GLint i = 0; i < count; ++i) {
        GLsizei length = 0; GLint size = 0; GLenum candidate = 0;
        glGetActiveUniform(program, i, name.size(), &length, &size, &candidate, name.data());
        if (glGetUniformLocation(program, name.data()) == location) { type = candidate; break; }
    }
    switch (type) {
    case GL_FLOAT_VEC2: case GL_INT_VEC2: case GL_UNSIGNED_INT_VEC2: case GL_BOOL_VEC2:
    case GL_DOUBLE_VEC2: return 2;
    case GL_FLOAT_VEC3: case GL_INT_VEC3: case GL_UNSIGNED_INT_VEC3: case GL_BOOL_VEC3:
    case GL_DOUBLE_VEC3: return 3;
    case GL_FLOAT_VEC4: case GL_INT_VEC4: case GL_UNSIGNED_INT_VEC4: case GL_BOOL_VEC4:
    case GL_DOUBLE_VEC4: return 4;
    case GL_FLOAT_MAT2: case GL_DOUBLE_MAT2: return 4;
    case GL_FLOAT_MAT3: case GL_DOUBLE_MAT3: return 9;
    case GL_FLOAT_MAT4: case GL_DOUBLE_MAT4: return 16;
    case GL_FLOAT_MAT2x3: case GL_FLOAT_MAT3x2:
    case GL_DOUBLE_MAT2x3: case GL_DOUBLE_MAT3x2: return 6;
    case GL_FLOAT_MAT2x4: case GL_FLOAT_MAT4x2:
    case GL_DOUBLE_MAT2x4: case GL_DOUBLE_MAT4x2: return 8;
    case GL_FLOAT_MAT3x4: case GL_FLOAT_MAT4x3:
    case GL_DOUBLE_MAT3x4: case GL_DOUBLE_MAT4x3: return 12;
    default: return 1;
    }
}

/* For the generated compatibility-profile cases (gexgl-compat-host.h). */
#define GEXGEN_NAME(kind, name) lookup(kind, name)
void* gexgen_proc(const char* name, const char* alias) {
    void* f = reinterpret_cast<void*>(_mesa_glapi_get_proc_address(name));
    if (!f && alias) f = reinterpret_cast<void*>(_mesa_glapi_get_proc_address(alias));
    return f;
}
void gexgen_missing(const char* name) {
    static std::unordered_set<std::string> said;
    if (said.insert(name).second)
        fprintf(stderr, "gexgl: Mesa has no %s; the call does nothing\n", name);
}

#include "gl-compat-generated-decoders.inc"

/* A desktop shader whose #version is above what this context compiles.
 *
 * st/mesa caps the GLSL version at the GL version the driver earns (4.00
 * on gex_wgpu today), while applications ask for what their content was
 * written for: Unity's HLSLcc emits "#version 410" for a geometry stage
 * (a game's grass) and up to "#version 460" where the host offers it. Refused,
 * the program fails to link and the draw goes ahead with whatever program
 * was bound -- a field of grass becomes its input points. Most of what
 * 4.10-4.60 add to the language is also available as extensions this
 * context has, so the source is compiled as the highest version it accepts
 * with those enabled; anything it really uses that is missing still fails
 * to compile, by name. GEXGL_NO_VERSION_FIT=1 keeps the source as sent. */
std::string fit_glsl_version(std::string src) {
    static int off = -1;
    if (off < 0) off = getenv("GEXGL_NO_VERSION_FIT") ? 1 : 0;
    if (off) return src;
    size_t at = src.find_first_not_of(" \t\r\n");
    if (at == std::string::npos || src.compare(at, 8, "#version") != 0) return src;
    const size_t eol = src.find('\n', at);
    const std::string line = src.substr(at, eol == std::string::npos ? std::string::npos : eol - at);
    unsigned want = 0;
    char profile[32] = "";
    if (std::sscanf(line.c_str(), "#version %u %31s", &want, profile) < 1) return src;
    if (std::strcmp(profile, "es") == 0 || want <= 400 || want > 460) return src;
    const char* lang = reinterpret_cast<const char*>(glGetString(GL_SHADING_LANGUAGE_VERSION));
    unsigned major = 0, minor = 0;
    if (!lang || std::sscanf(lang, "%u.%u", &major, &minor) != 2) return src;
    const unsigned have = major * 100 + minor;
    if (have >= want || have < 400) return src;
    std::string head = "#version " + std::to_string(have);
    if (profile[0]) head += std::string(" ") + profile;
    head += "\n";
    static const char* const extensions[] = {
        "GL_ARB_separate_shader_objects", "GL_ARB_shading_language_420pack",
        "GL_ARB_explicit_attrib_location", "GL_ARB_explicit_uniform_location",
        "GL_ARB_gpu_shader5", "GL_ARB_texture_query_levels",
        "GL_ARB_shader_image_load_store", "GL_ARB_arrays_of_arrays",
        "GL_ARB_enhanced_layouts", "GL_ARB_shader_storage_buffer_object",
        "GL_ARB_shader_bit_encoding", "GL_ARB_shading_language_packing",
        "GL_ARB_texture_cube_map_array", "GL_ARB_texture_gather",
        "GL_ARB_derivative_control", "GL_ARB_shader_texture_image_samples",
        "GL_ARB_cull_distance", "GL_ARB_conservative_depth",
        "GL_ARB_fragment_layer_viewport", "GL_ARB_viewport_array",
    };
    for (const char* e : extensions) head += std::string("#extension ") + e + " : enable\n";
    /* The original line stays as a comment, so line numbers shift by the
     * extension count and an error still shows what was asked for. */
    head += "// " + line + "\n";
    static unsigned said;
    if (said++ < 4)
        fprintf(stderr, "gexgl: \"%s\" compiled as GLSL %u with extensions\n", line.c_str(), have);
    return src.substr(0, at) + head + (eol == std::string::npos ? std::string() : src.substr(eol + 1));
}

bool one(uint32_t op, Reader& a) {
    switch (op) {
    case CONTEXT_CREATE: {
        // The client API the application selected with eglBindAPI, and the
        // version it asked for. Only the guest knows these, which is why they
        // are on the wire; see crates/gex-gl/src/proto.rs.
        const uint32_t requested_api = a.u32();
        const uint32_t requested_major = a.u32();
        const uint32_t requested_minor = a.u32();
        /* Optional, and added later than the three above. Reading past the
         * end sets `ok`, which would make a guest that predates this field
         * fail to get a context at all -- so the flag is put back and the
         * absent value read as zero, "let the host choose". Staging a stale
         * guest driver beside a current host has happened here before. */
        const bool had_required_args = a.ok;
        const uint32_t requested_profile = a.u32();
        if (had_required_args && !a.ok) a.ok = true;
        if (!a.ok) return false;
        /* Both Mesa backends -- softpipe and gex_wgpu -- make their context
         * here, now that the guest has said what it is for. */
        {
            const int made = create_backend_context(requested_api, requested_major,
                                                    requested_minor, requested_profile);
            if (made != 0) {
                /* Recorded as itself rather than left to be reported as an
                 * unhandled opcode, which is what a false return from here
                 * looks like from outside: "opcode 1, unknown opcode" for a
                 * context that could not be created is a diagnosis that
                 * sends the reader to the wrong place entirely. */
                last_reject_op = op;
                last_reject_why = 4;
                last_reject_payload = (uint32_t)(-made);
                last_reject_client = current_client;
                last_reject_detailed = true;
                fprintf(stderr, "gexgl: client %u has no context: %d\n",
                        current_client, made);
                return false;
            }
        }
        // CONTEXT_CREATE is the lifetime boundary for a guest process.  The WebGPU
        // device is intentionally persistent, but GL object names and state are not.
        // Keeping these maps across runs creates duplicate reverse aliases whenever
        // the host GL recycles a name, sending queries back with another process's
        // guest name and retaining all of the old process's GPU resources.
        reset_guest_namespace();
        const char* vendor = reinterpret_cast<const char*>(glGetString(GL_VENDOR));
        const char* renderer = reinterpret_cast<const char*>(glGetString(GL_RENDERER));
        // Desktop applications commonly parse GL_VERSION to select driver
        // workarounds, and programs such as STK only recognise the
        // conventional Mesa, NVIDIA, Intel and ATI layouts. Mesa's own string
        // is one of those, so it is passed through unchanged.
        const char* version = reinterpret_cast<const char*>(glGetString(GL_VERSION));
        /* Said once. An application chooses its renderer by parsing this,
         * and a string that arrives empty is invisible from the guest's
         * side -- it looks like a driver with no name and no version. */
        {
            static bool said_strings;
            if (!said_strings) {
                said_strings = true;
                fprintf(stderr, "gexgl: context strings vendor=%s renderer=%s"
                                " version=%s\n",
                        vendor ? vendor : "(null)",
                        renderer ? renderer : "(null)",
                        version ? version : "(null)");
            }
        }
        put_blob(vendor, vendor ? std::strlen(vendor) : 0);
        put_blob(renderer, renderer ? std::strlen(renderer) : 0);
        put_blob(version, version ? std::strlen(version) : 0);
        break;
    }
    case SURFACE_RESIZE: {
        const int width = a.i32(), height = a.i32();
        resize_drawable(width, height);
        break;
    }
    /* Which window on the page's screen the frames after this one belong
     * to: see `op::SURFACE_ATTACH`. Kept, not acted on -- the page reads it
     * and the compositor does the rest, because what a surface id means is
     * the compositor's business and not this decoder's. */
    case SURFACE_ATTACH: {
        const unsigned kind = a.u32();
        const unsigned id = a.u32();
        const unsigned token = a.u32();
        const unsigned handle = a.u32();
        if (!a.ok) break;
        attached_kind = kind;
        attached_id = id;
        attached_token = token;
        attached_generation++;
        /* A window names the drawable its frames go to, and attaching makes
         * that drawable current: this is `eglCreateWindowSurface` finishing,
         * and what the client draws next belongs to this window. */
        use_surface(handle);
        sur->kind = kind;
        sur->id = id;
        sur->token = token;
        ++sur->generation;
        break;
    }
    case SURFACE_CURRENT: {
        const unsigned handle = a.u32();
        if (!a.ok) break;
        use_surface(handle);
        break;
    }
    case CONTEXT_DESTROY: {
        /* Everything this client's context held goes with it: its objects
         * are named in maps that mean nothing to the next context, and the
         * next `CONTEXT_CREATE` is then a creation rather than a no-op --
         * which is what lets a toolkit read the GL strings through a
         * throwaway context and still get the profile it asks for after. */
        if (mesa_context) {
            st_destroy_context(mesa_context);
            mesa_context = nullptr;
        }
        reset_guest_namespace();
        for (auto& entry : surfaces) {
            for (struct pipe_resource*& attachment : entry.second.drawable.attachments)
                pipe_resource_reference(&attachment, nullptr);
            pipe_resource_reference(&entry.second.shown, nullptr);
        }
        break;
    }
    case SURFACE_DETACH: {
        const unsigned handle = a.u32();
        if (!a.ok) break;
        drop_surface(handle);
        /* The last thing the page was told about is gone; saying so keeps a
         * compositor from drawing a window's final frame for ever. */
        if (attached_id && handle == 0) attached_id = 0;
        break;
    }
    case FINISH: glFinish(); put_u32(0); break;
    /* The reply is the number of frames the *page* has put on screen, which
     * is not the number the guest has swapped: the backend renders into a
     * texture and copies it to the canvas only when the page's animation loop
     * calls display(). The guest paces itself against the difference -- see
     * eglSwapBuffers -- because without that an application free-runs, and
     * the round trips alone starve the loop it is waiting for. */
    /* The reply is this *surface's* count of frames shown, which is what the
     * guest paces itself against: a client swapping one window must not be
     * released by another window's frame reaching the screen. */
    case PRESENT:
        present_backend();
        if (gl_counts()) {
            ++gl_client_counts[current_client].second;
            gl_count_line("present");
        }
        put_u32(sur ? sur->displayed : displayed);
        break;
    case CLEAR_COLOR: {
        for (float& value : current_clear_color) value = a.f32();
        glClearColor(current_clear_color[0], current_clear_color[1],
                     current_clear_color[2], current_clear_color[3]); break;
    }
    case CLEAR: glClear(a.u32()); break;
    case VIEWPORT: { GLint x=a.i32(), y=a.i32(); GLsizei w=a.i32(), h=a.i32(); glViewport(x,y,w,h); break; }
    case SCISSOR: { GLint x=a.i32(), y=a.i32(); GLsizei w=a.i32(), h=a.i32(); glScissor(x,y,w,h); break; }
    case ENABLE: glEnable(a.u32()); break;
    case DISABLE: glDisable(a.u32()); break;
    case BLEND_FUNC: { GLenum src=a.u32(), dst=a.u32(); glBlendFunc(src,dst); break; }
    case DEPTH_FUNC: glDepthFunc(a.u32()); break;
    case CULL_FACE: glCullFace(a.u32()); break;
    case FRONT_FACE: glFrontFace(a.u32()); break;
    case PIXEL_STOREI: {
        GLenum pname=a.u32(); GLint param=a.i32();
        glPixelStorei(pname,param);
        (void)gexgl_set_pixel_store(&pack_store, &unpack_store, pname, param);
        break;
    }
    case CLEAR_DEPTH: current_clear_depth=a.f32(); glClearDepthf(current_clear_depth); break;
    case DEPTH_MASK: glDepthMask(a.u32()); break;
    case HINT: { GLenum target=a.u32(), mode=a.u32(); glHint(target,mode); break; }
    case COLOR_MASK: { GLboolean r=a.u32(), g=a.u32(), b=a.u32(), alpha=a.u32(); glColorMask(r,g,b,alpha); break; }
    case BLEND_EQUATION: glBlendEquation(a.u32()); break;
    case BLEND_COLOR: { GLfloat r=a.f32(), g=a.f32(), b=a.f32(), alpha=a.f32(); glBlendColor(r,g,b,alpha); break; }
    case BLEND_EQUATION_SEPARATE: { GLenum rgb=a.u32(), alpha=a.u32(); glBlendEquationSeparate(rgb,alpha); break; }
    case CLEAR_BUFFER_FV: {
        GLenum b=a.u32(); GLint d=a.i32(); GLfloat v[4]; for(float& x:v)x=a.f32();
        glClearBufferfv(b,d,v);
        break;
    }
    case CLEAR_BUFFER_IV: {
        GLenum b=a.u32(); GLint d=a.i32(); GLint v[4]; for(int& x:v)x=a.i32();
        glClearBufferiv(b,d,v);
        break;
    }
    case CLEAR_BUFFER_UIV: {
        GLenum b=a.u32(); GLint d=a.i32(); GLuint v[4]; for(unsigned& x:v)x=a.u32();
        glClearBufferuiv(b,d,v);
        break;
    }
    case CLEAR_BUFFER_FI: {
        GLenum b=a.u32(); GLint d=a.i32(); GLfloat depth=a.f32(); GLint stencil=a.i32();
        glClearBufferfi(b,d,depth,stencil);
        break;
    }
    case STENCIL_FUNC: { GLenum func=a.u32(); GLint ref=a.i32(); GLuint mask=a.u32(); glStencilFunc(func,ref,mask); break; }
    case STENCIL_FUNC_SEPARATE: { GLenum face=a.u32(), func=a.u32(); GLint ref=a.i32(); GLuint mask=a.u32(); glStencilFuncSeparate(face,func,ref,mask); break; }
    case STENCIL_MASK: glStencilMask(a.u32()); break;
    case STENCIL_MASK_SEPARATE: { GLenum face=a.u32(); GLuint mask=a.u32(); glStencilMaskSeparate(face,mask); break; }
    case STENCIL_OP: { GLenum fail=a.u32(), zfail=a.u32(), zpass=a.u32(); glStencilOp(fail,zfail,zpass); break; }
    case STENCIL_OP_SEPARATE: { GLenum face=a.u32(), fail=a.u32(), zfail=a.u32(), zpass=a.u32(); glStencilOpSeparate(face,fail,zfail,zpass); break; }
    case BLEND_FUNC_SEPARATE: { GLenum srgb=a.u32(), drgb=a.u32(), sa=a.u32(), da=a.u32(); glBlendFuncSeparate(srgb,drgb,sa,da); break; }
    case POLYGON_OFFSET: { GLfloat factor=a.f32(), units=a.f32(); glPolygonOffset(factor,units); break; }
    case LINE_WIDTH: glLineWidth(a.f32()); break;
    case CLEAR_STENCIL: current_clear_stencil=a.i32(); glClearStencil(current_clear_stencil); break;
    case DEPTH_RANGE: { GLfloat near_value=a.f32(), far_value=a.f32(); glDepthRangef(near_value,far_value); break; }
    case GEN_BUFFERS: {
        GLsizei n = a.i32();
        if (gexgl_negative_count(n, [](GLsizei c, GLuint *p) { glGenBuffers(c, p); })) break;
        std::vector<GLuint> ids(n); glGenBuffers(n, ids.data());
        for (uint32_t i = 0; i < n; ++i) buffers[a.u32()] = ids[i]; break;
    }
    case DELETE_BUFFERS: {
        GLsizei n = a.i32();
        if (gexgl_negative_count(n, [](GLsizei c, GLuint *p) { glDeleteBuffers(c, p); })) break;
        std::vector<GLuint> ids; ids.reserve(n);
        for (GLsizei i = 0; i < n; ++i) { uint32_t g = a.u32(); ids.push_back(lookup(buffers, g)); buffers.erase(g); }
        glDeleteBuffers(n, ids.data()); break;
    }
    case GEN_QUERIES: {
        GLsizei n = a.i32();
        if (n < 0) { glGenQueries(n, nullptr); break; }
        std::vector<GLuint> ids(static_cast<size_t>(n)); glGenQueries(n, ids.data());
        for (GLsizei i = 0; i < n; ++i) queries[a.u32()] = ids[static_cast<size_t>(i)];
        break;
    }
    case DELETE_QUERIES: {
        GLsizei n = a.i32();
        if (n < 0) { glDeleteQueries(n, nullptr); break; }
        std::vector<GLuint> ids; ids.reserve(static_cast<size_t>(n));
        for (GLsizei i = 0; i < n; ++i) { uint32_t g=a.u32(); ids.push_back(lookup(queries,g)); queries.erase(g); }
        glDeleteQueries(n, ids.data()); break;
    }
    case IS_QUERY: put_u32(glIsQuery(lookup(queries, a.u32()))); break;
    case GEN_TRANSFORM_FEEDBACKS: {
        GLsizei n = a.i32();
        if (n < 0) { glGenTransformFeedbacks(n, nullptr); break; }
        std::vector<GLuint> ids(static_cast<size_t>(n)); glGenTransformFeedbacks(n, ids.data());
        for (GLsizei i = 0; i < n; ++i) transform_feedbacks[a.u32()] = ids[static_cast<size_t>(i)];
        break;
    }
    case DELETE_TRANSFORM_FEEDBACKS: {
        GLsizei n = a.i32();
        if (n < 0) { glDeleteTransformFeedbacks(n, nullptr); break; }
        std::vector<GLuint> ids; ids.reserve(static_cast<size_t>(n));
        for (GLsizei i = 0; i < n; ++i) { uint32_t g=a.u32(); ids.push_back(lookup(transform_feedbacks,g)); transform_feedbacks.erase(g); }
        glDeleteTransformFeedbacks(n, ids.data()); break;
    }
    case BIND_TRANSFORM_FEEDBACK: {
        GLenum target=a.u32(); GLuint id=lookup(transform_feedbacks,a.u32());
        glBindTransformFeedback(target,id); break;
    }
    case IS_TRANSFORM_FEEDBACK: put_u32(glIsTransformFeedback(lookup(transform_feedbacks,a.u32()))); break;
    case PAUSE_TRANSFORM_FEEDBACK: glPauseTransformFeedback(); break;
    case RESUME_TRANSFORM_FEEDBACK: glResumeTransformFeedback(); break;
    case BIND_BUFFER: {
        GLenum t = a.u32();
        glBindBuffer(t, lookup_bindable(buffers, a.u32()));
        break;
    }
    case BUFFER_DATA: {
        GLenum t = a.u32(), usage = a.u32(); uint64_t size = a.u64(); bool hasData = a.u32() != 0;
        uint32_t n = 0; const uint8_t* p = a.blob(n);
        if (size > static_cast<uint64_t>(std::numeric_limits<GLsizeiptr>::max()) ||
            (hasData && size != n) || (!hasData && n != 0)) { a.ok = false; break; }
        /* Re-specification of a vertex buffer, per buffer name.
         *
         * A 44-byte buffer bound with a 44-byte stride and drawn with ten
         * indices is either an application asking for exactly one vertex or a
         * later, larger glBufferData that did not take effect. Recording the
         * size each name is given, and saying so when it changes, separates
         * the two. Capped so a buffer re-specified every frame does not become
         * the log. */
        if (t == GL_ARRAY_BUFFER) {
            GLint bound = 0;
            glGetIntegerv(GL_ARRAY_BUFFER_BINDING, &bound);
            static std::unordered_map<GLint, uint64_t> vb_size;
            static unsigned told;
            auto prev = vb_size.find(bound);
            const bool respec = prev != vb_size.end() && prev->second != size;
            if ((respec || prev == vb_size.end()) && told < 24) {
                ++told;
                emscripten_log(EM_LOG_WARN,
                               "gex-gl: glBufferData ARRAY_BUFFER buf=%d "
                               "size=%llu (was %lld) data=%d usage=0x%x",
                               (int)bound, (unsigned long long)size,
                               respec ? (long long)prev->second : -1,
                               (int)hasData, usage);
            }
            vb_size[bound] = size;
        }
        glBufferData(t, static_cast<GLsizeiptr>(size), hasData ? p : nullptr, usage); break;
    }
    case BUFFER_SUB_DATA: {
        GLenum t = a.u32(); uint64_t offset = a.u64(), size = a.u64();
        uint32_t n = 0; const uint8_t* p = a.blob(n);
        if (offset > static_cast<uint64_t>(std::numeric_limits<GLintptr>::max()) ||
            size > static_cast<uint64_t>(std::numeric_limits<GLsizeiptr>::max()) || size != n) {
            a.ok = false; break;
        }
        glBufferSubData(t, static_cast<GLintptr>(offset), static_cast<GLsizeiptr>(size), p); break;
    }
    case BIND_BUFFER_BASE: {
        GLenum target = a.u32(); GLuint index = a.u32();
        glBindBufferBase(target, index, lookup(buffers, a.u32())); break;
    }
    case GEN_SAMPLERS: {
        GLsizei n = a.i32();
        if (gexgl_negative_count(n, [](GLsizei c, GLuint *p) { glGenSamplers(c, p); })) break;
        std::vector<GLuint> ids(n); glGenSamplers(n, ids.data());
        for (uint32_t i = 0; i < n; ++i) samplers[a.u32()] = ids[i]; break;
    }
    case DELETE_SAMPLERS: {
        uint32_t n = a.u32(); std::vector<GLuint> ids; ids.reserve(n);
        for (uint32_t i = 0; i < n; ++i) { uint32_t g=a.u32(); ids.push_back(lookup(samplers,g)); samplers.erase(g); }
        glDeleteSamplers(n, ids.data()); break;
    }
    case BIND_SAMPLER: { GLuint unit=a.u32(); glBindSampler(unit, lookup(samplers,a.u32())); break; }
    case SAMPLER_PARAMETER_I: {
        GLuint sampler = lookup(samplers, a.u32()); GLenum pname = a.u32();
        GLint values[4]{}; const size_t count = pname == GL_TEXTURE_BORDER_COLOR ? 4u : 1u;
        for (size_t i = 0; i < count; ++i) values[i] = a.i32();
        if (count == 4) glSamplerParameteriv(sampler, pname, values);
        else glSamplerParameteri(sampler, pname, values[0]);
        break;
    }
    case SAMPLER_PARAMETER_F: {
        GLuint sampler = lookup(samplers, a.u32()); GLenum pname = a.u32();
        GLfloat values[4]{}; const size_t count = pname == GL_TEXTURE_BORDER_COLOR ? 4u : 1u;
        for (size_t i = 0; i < count; ++i) values[i] = a.f32();
        if (count == 4) glSamplerParameterfv(sampler, pname, values);
        else glSamplerParameterf(sampler, pname, values[0]);
        break;
    }
    case CREATE_SHADER: { uint32_t g = a.u32(); shaders[g] = glCreateShader(a.u32()); break; }
    case SHADER_SOURCE: {
        GLuint s = lookup(shaders, a.u32()); std::string src = fit_glsl_version(a.string()); const char* p = src.c_str();
        shader_source_heads[s] = src.substr(0, 512);
        glShaderSource(s, 1, &p, nullptr); break;
    }
    case COMPILE_SHADER: glCompileShader(lookup(shaders, a.u32())); break;
    case DELETE_SHADER: {
        /* GL keeps a deleted shader that is still attached to a program,
         * and its name stays valid until it is detached: a program may
         * attach, delete, and only then compile and link. So the guest name
         * keeps its mapping; CREATE_SHADER given the same name replaces it. */
        glDeleteShader(lookup(shaders, a.u32()));
        break;
    }
    case CREATE_PROGRAM: { uint32_t g = a.u32(); programs[g] = glCreateProgram(); break; }
    case ATTACH_SHADER: { GLuint p = lookup(programs, a.u32()); glAttachShader(p, lookup(shaders, a.u32())); break; }
    case DETACH_SHADER: { GLuint p = lookup(programs, a.u32()); glDetachShader(p, lookup(shaders, a.u32())); break; }
    case BIND_ATTRIB_LOCATION: { GLuint p = lookup(programs, a.u32()); GLuint i = a.u32(); std::string n = a.string(); glBindAttribLocation(p, i, n.c_str()); break; }
    case LINK_PROGRAM: glLinkProgram(lookup(programs, a.u32())); break;
    case USE_PROGRAM: {
        const uint32_t next = a.u32();
        glUseProgram(lookup(programs, next));
        const uint32_t previous = current_guest_program;
        current_guest_program = next;
        if (previous != next && delete_pending_programs.erase(previous) != 0) {
            programs.erase(previous);
        }
        break;
    }
    case DELETE_PROGRAM: {
        const uint32_t g = a.u32();
        const auto it = programs.find(g);
        glDeleteProgram(it == programs.end() ? 0 : it->second);
        if (it != programs.end()) {
            if (g == current_guest_program) delete_pending_programs.insert(g);
            else programs.erase(it);
        }
        break;
    }
    case GET_UNIFORM_LOCATION: { GLuint p = lookup(programs, a.u32()); std::string n = a.string(); put_u32(glGetUniformLocation(p, n.c_str())); break; }
    case GET_ATTRIB_LOCATION: { GLuint p = lookup(programs, a.u32()); std::string n = a.string(); put_u32(glGetAttribLocation(p, n.c_str())); break; }
    case GET_UNIFORM_BLOCK_INDEX: { GLuint p = lookup(programs, a.u32()); std::string n = a.string(); put_u32(glGetUniformBlockIndex(p, n.c_str())); break; }
    case UNIFORM_BLOCK_BINDING: { GLuint p = lookup(programs, a.u32()); GLuint i = a.u32(); glUniformBlockBinding(p, i, a.u32()); break; }
    case GET_ATTACHED_SHADERS: {
        uint32_t gp = a.u32(), max_count = a.u32(); GLuint p = lookup(programs, gp);
        std::vector<GLuint> ids(max_count); GLsizei count = 0;
        glGetAttachedShaders(p, max_count, &count, ids.data());
        put_u32(count);
        for (GLsizei i = 0; i < count; ++i) {
            uint32_t guest = 0;
            for (const auto &entry : shaders) if (entry.second == ids[i]) { guest = entry.first; break; }
            put_u32(guest);
        }
        break;
    }
    case GET_SHADER_IV: {
        GLuint s = lookup(shaders, a.u32()); GLenum q = a.u32(); GLint v = 0; glGetShaderiv(s, q, &v);
        if (q == GL_COMPILE_STATUS && !v) { GLint n=0; glGetShaderiv(s,GL_INFO_LOG_LENGTH,&n); std::vector<char> log(std::max(n,1)); glGetShaderInfoLog(s,log.size(),nullptr,log.data()); emscripten_log(EM_LOG_ERROR,"gex-gl shader compile: %s",log.data());
            auto head = shader_source_heads.find(s);
            if (head != shader_source_heads.end()) {
                /* Line by line, and numbered: the compiler reports "0:LINE(COL)"
                 * and a single blob makes the reader count. */
                unsigned line = 1;
                size_t at = 0;
                while (at < head->second.size() && line <= 10) {
                    size_t nl = head->second.find('\n', at);
                    if (nl == std::string::npos) nl = head->second.size();
                    emscripten_log(EM_LOG_ERROR, "gex-gl shader src %u: %s",
                                   line, head->second.substr(at, nl - at).c_str());
                    at = nl + 1;
                    ++line;
                }
            } }
        put_u32(v); break;
    }
    case GET_PROGRAM_IV: {
        GLuint p = lookup(programs, a.u32()); GLenum q = a.u32(); GLint v = 0; glGetProgramiv(p, q, &v);
        if (q == GL_LINK_STATUS && !v) { GLint n=0; glGetProgramiv(p,GL_INFO_LOG_LENGTH,&n); std::vector<char> log(std::max(n,1)); glGetProgramInfoLog(p,log.size(),nullptr,log.data()); emscripten_log(EM_LOG_ERROR,"gex-gl program link: %s",log.data()); }
        put_u32(v); break;
    }
    case GET_SHADER_INFO_LOG: {
        GLuint s = lookup(shaders, a.u32()); GLint n = 0; glGetShaderiv(s, GL_INFO_LOG_LENGTH, &n);
        std::vector<char> out(std::max(n, 1)); GLsizei got = 0; glGetShaderInfoLog(s, out.size(), &got, out.data()); put_blob(out.data(), got); break;
    }
    case GET_PROGRAM_INFO_LOG: {
        GLuint p = lookup(programs, a.u32()); GLint n = 0; glGetProgramiv(p, GL_INFO_LOG_LENGTH, &n);
        std::vector<char> out(std::max(n, 1)); GLsizei got = 0; glGetProgramInfoLog(p, out.size(), &got, out.data()); put_blob(out.data(), got); break;
    }
    case UNIFORM_1I: { GLint l=a.i32(), x=a.i32(); glUniform1i(l,x); break; }
    case UNIFORM_2I: { GLint l=a.i32(), x=a.i32(), y=a.i32(); glUniform2i(l,x,y); break; }
    case UNIFORM_3I: { GLint l=a.i32(), x=a.i32(), y=a.i32(), z=a.i32(); glUniform3i(l,x,y,z); break; }
    case UNIFORM_4I: { GLint l=a.i32(), x=a.i32(), y=a.i32(), z=a.i32(), w=a.i32(); glUniform4i(l,x,y,z,w); break; }
    case UNIFORM_1F: { GLint l=a.i32(); GLfloat x=a.f32(); glUniform1f(l,x); break; }
    case UNIFORM_2F: { GLint l=a.i32(); GLfloat x=a.f32(), y=a.f32(); glUniform2f(l,x,y); break; }
    case UNIFORM_3F: { GLint l=a.i32(); GLfloat x=a.f32(), y=a.f32(), z=a.f32(); glUniform3f(l,x,y,z); break; }
    case UNIFORM_4F: { GLint l=a.i32(); GLfloat x=a.f32(), y=a.f32(), z=a.f32(), w=a.f32(); glUniform4f(l,x,y,z,w); break; }
    case UNIFORM_MATRIX4FV: {
        GLint loc = a.i32(); GLsizei count = a.u32(); GLboolean transpose = a.u32(); uint32_t n; const uint8_t* p = a.blob(n);
        glUniformMatrix4fv(loc, count, transpose, reinterpret_cast<const GLfloat*>(p)); break;
    }
    case GET_ACTIVE_UNIFORM:
    case GET_ACTIVE_ATTRIB:
    case GET_TRANSFORM_FEEDBACK_VARYING: {
        GLuint program = lookup(programs, a.u32()); GLuint index = a.u32();
        GLenum length_query = op == GET_ACTIVE_UNIFORM
            ? GL_ACTIVE_UNIFORM_MAX_LENGTH
            : op == GET_ACTIVE_ATTRIB
            ? GL_ACTIVE_ATTRIBUTE_MAX_LENGTH
            : GL_TRANSFORM_FEEDBACK_VARYING_MAX_LENGTH;
        GLint max_len = 0;
        glGetProgramiv(program, length_query, &max_len);
        std::vector<char> name(std::max(max_len, 1)); GLsizei length = 0; GLint size = 0; GLenum type = 0;
        if (op == GET_ACTIVE_UNIFORM) glGetActiveUniform(program, index, name.size(), &length, &size, &type, name.data());
        else if (op == GET_ACTIVE_ATTRIB) glGetActiveAttrib(program, index, name.size(), &length, &size, &type, name.data());
        else glGetTransformFeedbackVarying(program, index, name.size(), &length, &size, &type, name.data());
        put_u32(size); put_u32(type); put_blob(name.data(), length); break;
    }
    case GET_ACTIVE_UNIFORM_NAME:
    case GET_ACTIVE_UNIFORM_BLOCK_NAME: {
        GLuint program = lookup(programs, a.u32()); GLuint index = a.u32();
        GLint max_len = 0;
        glGetProgramiv(program,
                       op == GET_ACTIVE_UNIFORM_NAME
                           ? GL_ACTIVE_UNIFORM_MAX_LENGTH
                           : GL_ACTIVE_UNIFORM_BLOCK_MAX_NAME_LENGTH,
                       &max_len);
        std::vector<char> name(std::max(max_len, 1));
        GLsizei length = 0;
        if (op == GET_ACTIVE_UNIFORM_NAME)
            glGetActiveUniformName(program, index, name.size(), &length, name.data());
        else
            glGetActiveUniformBlockName(program, index, name.size(), &length, name.data());
        put_blob(name.data(), length); break;
    }
    case GET_ACTIVE_UNIFORM_BLOCKIV: {
        GLuint program = lookup(programs, a.u32());
        GLuint index = a.u32();
        GLenum pname = a.u32();
        /* Only one query returns more than a single integer, and its length is
         * itself a query. */
        GLint count = 1;
        if (pname == GL_UNIFORM_BLOCK_ACTIVE_UNIFORM_INDICES)
            glGetActiveUniformBlockiv(program, index,
                                      GL_UNIFORM_BLOCK_ACTIVE_UNIFORMS, &count);
        std::vector<GLint> params(std::max(count, 1), 0);
        glGetActiveUniformBlockiv(program, index, pname, params.data());
        put_blob(params.data(), std::max(count, 0) * sizeof(GLint)); break;
    }
    case GET_ACTIVE_UNIFORMSIV: {
        GLuint program = lookup(programs, a.u32());
        GLenum pname = a.u32();
        uint32_t bytes = 0;
        const uint8_t* raw = a.blob(bytes);
        const GLsizei count = (GLsizei)(bytes / sizeof(GLuint));
        std::vector<GLuint> indices(count);
        if (count) memcpy(indices.data(), raw, (size_t)count * sizeof(GLuint));
        std::vector<GLint> params(std::max(count, 1), 0);
        glGetActiveUniformsiv(program, count, indices.data(), pname, params.data());
        put_blob(params.data(), (size_t)count * sizeof(GLint)); break;
    }
    case GET_UNIFORM_INDICES: {
        GLuint program = lookup(programs, a.u32());
        const GLsizei count = (GLsizei)a.u32();
        uint32_t bytes = 0;
        const uint8_t* raw = a.blob(bytes);
        /* One blob of NUL-terminated names; walk it rather than trusting a
         * separate length array the guest would have had to build. */
        std::vector<const GLchar*> names;
        const char* at = reinterpret_cast<const char*>(raw);
        const char* end = at + bytes;
        for (GLsizei i = 0; i < count && at < end; ++i) {
            names.push_back(at);
            at += strnlen(at, (size_t)(end - at)) + 1;
        }
        std::vector<GLuint> indices(std::max<size_t>(names.size(), 1),
                                    GL_INVALID_INDEX);
        if (!names.empty())
            glGetUniformIndices(program, (GLsizei)names.size(), names.data(),
                                indices.data());
        put_blob(indices.data(), names.size() * sizeof(GLuint)); break;
    }
    case GET_SHADER_SOURCE: {
        GLuint shader = lookup(shaders, a.u32());
        GLint max_len = 0;
        glGetShaderiv(shader, GL_SHADER_SOURCE_LENGTH, &max_len);
        std::vector<char> source(std::max(max_len, 1));
        GLsizei length = 0;
        glGetShaderSource(shader, source.size(), &length, source.data());
        put_blob(source.data(), length); break;
    }
#define GEX_UNIFORM_VECTOR(OP, FN, TYPE) case OP: { GLint l=a.i32(); GLsizei n=a.i32(); uint32_t bytes; const uint8_t*p=a.blob(bytes); FN(l,n,reinterpret_cast<const TYPE*>(p)); break; }
    GEX_UNIFORM_VECTOR(UNIFORM_1IV, glUniform1iv, GLint)
    GEX_UNIFORM_VECTOR(UNIFORM_1FV, glUniform1fv, GLfloat)
    GEX_UNIFORM_VECTOR(UNIFORM_2FV, glUniform2fv, GLfloat)
    GEX_UNIFORM_VECTOR(UNIFORM_3FV, glUniform3fv, GLfloat)
    GEX_UNIFORM_VECTOR(UNIFORM_4FV, glUniform4fv, GLfloat)
    GEX_UNIFORM_VECTOR(UNIFORM_2IV, glUniform2iv, GLint)
    GEX_UNIFORM_VECTOR(UNIFORM_3IV, glUniform3iv, GLint)
    GEX_UNIFORM_VECTOR(UNIFORM_4IV, glUniform4iv, GLint)
#undef GEX_UNIFORM_VECTOR
    case UNIFORM_MATRIX2FV: {GLint l=a.i32();GLsizei n=a.i32();GLboolean t=a.u32();uint32_t bytes;const uint8_t*p=a.blob(bytes);glUniformMatrix2fv(l,n,t,reinterpret_cast<const GLfloat*>(p));break;}
    case UNIFORM_MATRIX3FV: {GLint l=a.i32();GLsizei n=a.i32();GLboolean t=a.u32();uint32_t bytes;const uint8_t*p=a.blob(bytes);glUniformMatrix3fv(l,n,t,reinterpret_cast<const GLfloat*>(p));break;}
    case ENABLE_VERTEX_ATTRIB_ARRAY: glEnableVertexAttribArray(a.u32()); break;
    case DISABLE_VERTEX_ATTRIB_ARRAY: glDisableVertexAttribArray(a.u32()); break;
    case VERTEX_ATTRIB_POINTER: {
        GLuint i = a.u32(); GLint size = a.i32(); GLenum type = a.u32(); GLboolean norm = a.u32(); GLsizei stride = a.i32(); uintptr_t off = a.u32();
        glVertexAttribPointer(i, size, type, norm, stride, reinterpret_cast<void*>(off)); break;
    }
    case VERTEX_ATTRIB_I_POINTER: {
        GLuint i = a.u32(); GLint size = a.i32(); GLenum type = a.u32();
        GLsizei stride = a.i32(); uintptr_t off = a.u32();
        glVertexAttribIPointer(i, size, type, stride, reinterpret_cast<void*>(off)); break;
    }
    case VERTEX_ATTRIB_DIVISOR: { GLuint index=a.u32(), divisor=a.u32();
        /* Whether the guest asks for instancing at all. The scene shader reads
         * per-instance attributes and Gallium reports every element with
         * divisor 0, so either the application never sets one or it is lost on
         * the way here. One line per (index, divisor). */
        { static std::unordered_map<uint64_t, unsigned> seen;
          if (seen[((uint64_t)index << 32) | divisor]++ == 0)
            emscripten_log(EM_LOG_WARN, "gex-gl: glVertexAttribDivisor(%u, %u)",
                           index, divisor); }
        glVertexAttribDivisor(index,divisor); break; }
    case VERTEX_ATTRIB_4F: { GLuint index=a.u32(); GLfloat x=a.f32(), y=a.f32(), z=a.f32(), w=a.f32(); glVertexAttrib4f(index,x,y,z,w); break; }
    case VERTEX_ATTRIB_I4I: { GLuint index=a.u32(); GLint x=a.i32(), y=a.i32(), z=a.i32(), w=a.i32(); glVertexAttribI4i(index,x,y,z,w); break; }
    case GEN_VERTEX_ARRAYS: {
        GLsizei n = a.i32();
        if (gexgl_negative_count(n, [](GLsizei c, GLuint *p) { glGenVertexArrays(c, p); })) break;
        std::vector<GLuint> ids(n); glGenVertexArrays(n, ids.data());
        for(uint32_t i=0;i<n;++i) vertex_arrays[a.u32()]=ids[i]; break;
    }
    case DELETE_VERTEX_ARRAYS: {
        uint32_t n=a.u32(); std::vector<GLuint> ids; ids.reserve(n);
        for(uint32_t i=0;i<n;++i){uint32_t g=a.u32();ids.push_back(lookup(vertex_arrays,g));vertex_arrays.erase(g);} glDeleteVertexArrays(n,ids.data());break;
    }
    case BIND_VERTEX_ARRAY: { uint32_t g=a.u32(); glBindVertexArray(g ? lookup(vertex_arrays,g) : default_vao); break; }
    case GEN_TEXTURES: {
        GLsizei n = a.i32();
        if (gexgl_negative_count(n, [](GLsizei c, GLuint *p) { glGenTextures(c, p); })) break;
        std::vector<GLuint> ids(n); glGenTextures(n, ids.data());
        for (uint32_t i = 0; i < n; ++i) {
            const uint32_t g = a.u32();
            textures[g] = ids[i];
        }
        break;
    }
    case DELETE_TEXTURES: {

        uint32_t n = a.u32(); std::vector<GLuint> ids; ids.reserve(n);
        for (uint32_t i = 0; i < n; ++i) { uint32_t g = a.u32();
            ids.push_back(lookup(textures, g)); textures.erase(g); }
        glDeleteTextures(n, ids.data()); break;
    }
    case BIND_TEXTURE: {
        GLenum t = a.u32();
        const uint32_t g = a.u32();
        /* GL creates a texture object when an unused non-zero name is bound;
           only glGen* reserving names is optional. Looking the name up and
           binding zero on a miss silently redirected every later call to the
           default texture of that target -- uploads appeared to succeed and
           the object never existed, which surfaces much later and elsewhere
           as an incomplete framebuffer. */
        if (g && !textures.count(g)) {
            GLuint created = 0;
            glGenTextures(1, &created);
            textures[g] = created;
        }
        glBindTexture(t, lookup(textures, g)); break; }
    case ACTIVE_TEXTURE: glActiveTexture(a.u32()); break;
    case TEX_PARAMETER_I: { GLenum target=a.u32(), pname=a.u32(); GLint param=a.i32(); glTexParameteri(target,pname,param); break; }
    case TEX_PARAMETER_F: { GLenum target=a.u32(), pname=a.u32(); GLfloat value=a.f32(); glTexParameterf(target,pname,value); break; }
    case TEX_PARAMETER_FV: {
        GLenum target=a.u32(), pname=a.u32(); GLfloat value[4]{};
        const size_t count = (pname == GL_TEXTURE_BORDER_COLOR || pname == GL_TEXTURE_SWIZZLE_RGBA) ? 4 : 1;
        for (size_t i = 0; i < count; ++i) value[i]=a.f32();
        glTexParameterfv(target,pname,value);
        break;
    }
    case TEX_PARAMETER_IV: {
        GLenum target=a.u32(), pname=a.u32(); GLint value[4]{};
        const size_t count = (pname == GL_TEXTURE_BORDER_COLOR || pname == GL_TEXTURE_SWIZZLE_RGBA) ? 4 : 1;
        for (size_t i = 0; i < count; ++i) value[i]=a.i32();
        glTexParameteriv(target,pname,value);
        break;
    }
    case TEX_IMAGE_2D: {
        GLenum target = a.u32(); GLint level = a.i32(), internal = a.i32();
        GLsizei w = a.i32(), h = a.i32(); GLint border = a.i32();
        GLenum format = a.u32(), type = a.u32(); bool pbo = a.u32() != 0;
        uintptr_t pointer = static_cast<uintptr_t>(a.u64());
        uint32_t n; const uint8_t* p = a.blob(n);
        gexgl_pixel_layout layout{};
        const bool valid = gexgl_compute_pixel_layout(format, type, w, h, -1,
                                                       &unpack_store, &layout);
        if ((pbo && n != 0) || (!pbo && pointer && (!valid || layout.end != n))) {
            a.ok = false; break;
        }
        const void* data = pbo ? reinterpret_cast<const void*>(pointer)
                               : (pointer ? p : nullptr);
        glTexImage2D(target, level, internal, w, h, border, format, type, data);
        break;
    }
    case TEX_SUB_IMAGE_2D: {
        GLenum target = a.u32(); GLint level = a.i32(), x = a.i32(), y = a.i32();
        GLsizei w = a.i32(), h = a.i32(); GLenum format = a.u32(), type = a.u32();
        bool pbo = a.u32() != 0; uintptr_t pointer = static_cast<uintptr_t>(a.u64());
        uint32_t n; const uint8_t* p = a.blob(n);
        gexgl_pixel_layout layout{};
        const bool valid = gexgl_compute_pixel_layout(format, type, w, h, -1,
                                                       &unpack_store, &layout);
        if ((pbo && n != 0) || (!pbo && pointer && (!valid || layout.end != n))) {
            a.ok = false; break;
        }
        const void* data = pbo ? reinterpret_cast<const void*>(pointer)
                               : (pointer ? p : nullptr);
        glTexSubImage2D(target, level, x, y, w, h, format, type, data);
        break;
    }
    case GEN_FRAMEBUFFERS: {
        GLsizei n = a.i32();
        if (gexgl_negative_count(n, [](GLsizei c, GLuint *p) { glGenFramebuffers(c, p); })) break;
        std::vector<GLuint> ids(n);glGenFramebuffers(n,ids.data());for(GLsizei i=0;i<n;++i)framebuffers[a.u32()]=ids[i];break;
    }
    case DELETE_FRAMEBUFFERS: {
        uint32_t n=a.u32();std::vector<GLuint> ids;ids.reserve(n);for(uint32_t i=0;i<n;++i){uint32_t g=a.u32();ids.push_back(lookup(framebuffers,g));framebuffers.erase(g);}glDeleteFramebuffers(n,ids.data());break;
    }
    case BIND_FRAMEBUFFER: {GLenum t=a.u32();glBindFramebuffer(t,lookup(framebuffers,a.u32()));break;}
    case CHECK_FRAMEBUFFER_STATUS: put_u32(glCheckFramebufferStatus(a.u32())); break;
    case FRAMEBUFFER_TEXTURE_2D: {
        GLenum t=a.u32(),att=a.u32(),tt=a.u32();
        const uint32_t guest_tex=a.u32();
        GLuint tex=lookup(textures,guest_tex);
        GLint level=a.i32();
        /* A name the map does not know looks up as zero, and zero means
           *detach* -- so a miss here silently produces a framebuffer with no
           attachment, which st/mesa reports as "no teximage" and the
           application sees only as GL_FRAMEBUFFER_INCOMPLETE_ATTACHMENT. */
        if (guest_tex && !tex) {
            static int said;
            if (said < 4) { said++;
                uint32_t lo = 0xffffffffu, hi = 0;
                for (const auto& kv : textures) {
                    if (kv.first < lo) lo = kv.first;
                    if (kv.first > hi) hi = kv.first;
                }
                std::fprintf(stderr,
                             "gexgl: framebuffer texture %u unknown, "
                             "attaching zero (detach); map has %zu entries "
                             "spanning %u..%u\n",
                             guest_tex, textures.size(),
                             textures.empty() ? 0 : lo,
                             textures.empty() ? 0 : hi);
            }
        }
        glFramebufferTexture2D(t,att,tt,tex,level);break;}
    case GEN_RENDERBUFFERS: {
        GLsizei n = a.i32();
        if (gexgl_negative_count(n, [](GLsizei c, GLuint *p) { glGenRenderbuffers(c, p); })) break;
        std::vector<GLuint> ids(n);glGenRenderbuffers(n,ids.data());for(GLsizei i=0;i<n;++i)renderbuffers[a.u32()]=ids[i];break;
    }
    case DELETE_RENDERBUFFERS: {
        uint32_t n=a.u32();std::vector<GLuint> ids;ids.reserve(n);for(uint32_t i=0;i<n;++i){uint32_t g=a.u32();ids.push_back(lookup(renderbuffers,g));renderbuffers.erase(g);}glDeleteRenderbuffers(n,ids.data());break;
    }
    case BIND_RENDERBUFFER: {GLenum t=a.u32();glBindRenderbuffer(t,lookup(renderbuffers,a.u32()));break;}
    case RENDERBUFFER_STORAGE: { GLenum target=a.u32(), format=a.u32(); GLsizei w=a.i32(), h=a.i32(); glRenderbufferStorage(target,format,w,h); break; }
    case DRAW_ARRAYS: { GLenum mode=a.u32(); GLint first=a.i32(); GLsizei count=a.i32(); glDrawArrays(mode,first,count); break; }
    case DRAW_ELEMENTS: {
        GLenum mode = a.u32(); GLsizei n = a.i32(); GLenum type = a.u32();
        uintptr_t off = a.u32();
        const bool client = a.u32() != 0;
        if (!client) {
            glDrawElements(mode, n, type, reinterpret_cast<void*>(off));
            break;
        }
        /* The indices came with the command because the guest had no element
           array buffer bound. Passing them straight through keeps the client
           array exactly as GL describes it; sending the guest's pointer as an
           offset had the host read its own address space instead. */
        uint32_t bytes = 0; const uint8_t *wire = a.blob(bytes);
        if (!wire) { a.ok = false; break; }
        glDrawElements(mode, n, type, wire);
        break;
    }
    case DRAW_BUFFERS: {
        GLsizei n = a.i32(); current_draw_buffers.resize(std::max(n, 0));
        for (GLsizei i = 0; i < n; ++i) current_draw_buffers[i] = a.u32();
        glDrawBuffers(n, current_draw_buffers.data()); break;
    }
    case DRAW_ARRAYS_INSTANCED: { GLenum mode=a.u32(); GLint first=a.i32(); GLsizei count=a.i32(), instances=a.i32(); glDrawArraysInstanced(mode,first,count,instances); break; }
    case DRAW_ELEMENTS_INSTANCED: {
        GLenum mode=a.u32(); GLsizei n=a.i32(); GLenum type=a.u32(); uintptr_t off=a.u32(); GLsizei instances=a.i32();
        glDrawElementsInstanced(mode,n,type,reinterpret_cast<void*>(off),instances); break;
    }
    case BLIT_FRAMEBUFFER: {
        GLint sx0=a.i32(),sy0=a.i32(),sx1=a.i32(),sy1=a.i32();
        GLint dx0=a.i32(),dy0=a.i32(),dx1=a.i32(),dy1=a.i32();
        GLbitfield mask=a.u32(); GLenum filter=a.u32();
        glBlitFramebuffer(sx0,sy0,sx1,sy1,dx0,dy0,dx1,dy1,mask,filter); break;
    }
    case FRAMEBUFFER_RENDERBUFFER: {GLenum t=a.u32(),att=a.u32(),rt=a.u32();glFramebufferRenderbuffer(t,att,rt,lookup(renderbuffers,a.u32()));break;}
    case GENERATE_MIPMAP: glGenerateMipmap(a.u32()); break;
    case TEX_STORAGE_2D: { GLenum target=a.u32(); GLsizei levels=a.i32(); GLenum format=a.u32(); GLsizei w=a.i32(), h=a.i32(); glTexStorage2D(target,levels,format,w,h); break; }
    case TEX_STORAGE_3D: { GLenum target=a.u32(); GLsizei levels=a.i32(); GLenum format=a.u32(); GLsizei w=a.i32(), h=a.i32(), d=a.i32(); glTexStorage3D(target,levels,format,w,h,d); break; }
    case TEX_IMAGE_3D: {
        GLenum t=a.u32();GLint l=a.i32(),in=a.i32();GLsizei w=a.i32(),h=a.i32(),d=a.i32();
        GLint border=a.i32();GLenum f=a.u32(),ty=a.u32();bool pbo=a.u32()!=0;
        uintptr_t pointer=static_cast<uintptr_t>(a.u64());uint32_t n;const uint8_t* bytes=a.blob(n);
        gexgl_pixel_layout layout{};
        const bool valid=gexgl_compute_pixel_layout(f,ty,w,h,d,&unpack_store,&layout);
        if ((pbo && n != 0) || (!pbo && pointer && (!valid || layout.end != n))) {
            a.ok=false;break;
        }
        const void* data=pbo?reinterpret_cast<const void*>(pointer):(pointer?bytes:nullptr);
        glTexImage3D(t,l,in,w,h,d,border,f,ty,data);break;
    }
    case FRAMEBUFFER_TEXTURE_LAYER: {GLenum t=a.u32(),att=a.u32();GLuint tex=lookup(textures,a.u32());GLint level=a.i32(),layer=a.i32();glFramebufferTextureLayer(t,att,tex,level,layer);break;}
    case READ_PIXELS: {
        GLint x = a.i32(), y = a.i32(); GLsizei w = a.i32(), h = a.i32();
        GLenum format = a.u32(), type = a.u32(); bool pbo = a.u32() != 0;
        uintptr_t pointer = static_cast<uintptr_t>(a.u64());
        if (pbo) {
            glReadPixels(x,y,w,h,format,type,reinterpret_cast<void*>(pointer));
            break;
        }
        gexgl_pixel_layout layout{};
        if (!gexgl_compute_pixel_layout(format,type,w,h,-1,&pack_store,&layout) ||
            layout.end > UINT32_MAX) {
            glReadPixels(x,y,w,h,format,type,nullptr);
            put_u32(0); put_blob(nullptr,0); break;
        }
        std::vector<uint8_t> px(layout.end, 0);
        glReadPixels(x,y,w,h,format,type,px.data());
        put_u32(static_cast<uint32_t>(layout.offset));
        put_blob(px.data()+layout.offset,
                 static_cast<uint32_t>(layout.end-layout.offset));
        break;
    }
    case GET_STRING: {
        GLenum name = a.u32();
        /* The real string, not a claim.  This used to answer GL_VERSION with a
         * fixed "3.3 (Core Profile)" regardless of the context that actually
         * came up, which -- combined with the guest substituting its own
         * version on top -- meant nothing in the chain reflected what the
         * backend could honour.  A guest that is told 4.4 and handed an ES 2.0
         * context discovers the difference one rejected call at a time. */
        const char* s = reinterpret_cast<const char*>(glGetString(name));
        put_blob(s, s ? std::strlen(s) : 0);
        break;
    }
    case GET_INTEGERV: {
        const GLenum pname = a.u32();
        const size_t n = integer_query_components(pname);
        /* Room for what GL writes even where the table says less. */
        std::vector<GLint> values(std::max<size_t>(n, 16));
        if (n) glGetIntegerv(pname, values.data());
        put_blob(values.data(), n * sizeof(values[0]));
        break;
    }
    case GET_ERROR: put_u32(glGetError()); break;
    case GET_STRING_I: {
        GLenum name = a.u32(); GLuint index = a.u32();
        const char* s = reinterpret_cast<const char*>(glGetStringi(name, index));
        put_blob(s, s ? std::strlen(s) : 0);
        break;
    }
    case GET_UNIFORM_FV:
    case GET_UNIFORM_IV: {
        GLuint program=lookup_program_or_shader(a.u32()); GLint location=a.i32();
        uint32_t components=uniform_components(program,location);
        if (op == GET_UNIFORM_FV) {
            GLfloat values[16] = {}; glGetUniformfv(program,location,values);
            put_blob(values,components*sizeof(values[0]));
        } else {
            GLint values[16] = {}; glGetUniformiv(program,location,values);
            put_blob(values,components*sizeof(values[0]));
        }
        break;
    }
    /* Fixed function.
     *
     * Only reachable in a compatibility profile, which is what a GLX context
     * gets -- these entry points do not exist in a core one, and Mesa will
     * raise GL_INVALID_OPERATION rather than crash if one arrives anyway.
     * There is nothing to translate: the host is real Mesa and has them. */
    case MATRIX_MODE: glMatrixMode(a.u32()); break;
    case LOAD_IDENTITY: glLoadIdentity(); break;
    case PUSH_MATRIX: glPushMatrix(); break;
    case POP_MATRIX: glPopMatrix(); break;
    case BEGIN: glBegin(a.u32()); break;
    case END: glEnd(); break;
    case SHADE_MODEL: glShadeModel(a.u32()); break;
    case END_LIST: glEndList(); break;
    case CALL_LIST: glCallList(a.u32()); break;
    case NEW_LIST: { GLuint list = a.u32(); glNewList(list, a.u32()); break; }
    case DELETE_LISTS: { GLuint list = a.u32(); glDeleteLists(list, a.i32()); break; }
    case TRANSLATEF: {
        const GLfloat x = a.f32(), y = a.f32(), z = a.f32();
        glTranslatef(x, y, z); break;
    }
    case ROTATEF: {
        const GLfloat angle = a.f32(), x = a.f32(), y = a.f32(), z = a.f32();
        glRotatef(angle, x, y, z); break;
    }
    case VERTEX3F: {
        const GLfloat x = a.f32(), y = a.f32(), z = a.f32();
        glVertex3f(x, y, z); break;
    }
    case NORMAL3F: {
        const GLfloat x = a.f32(), y = a.f32(), z = a.f32();
        glNormal3f(x, y, z); break;
    }
    case FRUSTUM: {
        /* Doubles, two words each: the projection this builds is the one
         * place in the set where float range is not enough. */
        const GLdouble l = a.f64(), r = a.f64(), b = a.f64();
        const GLdouble t = a.f64(), n = a.f64(), f = a.f64();
        glFrustum(l, r, b, t, n, f); break;
    }
    case LIGHTFV:
    case MATERIALFV: {
        const GLenum target = a.u32();
        const GLenum pname = a.u32();
        const uint32_t count = a.u32();
        /* The guest sends the count the pname calls for; four is the most any
         * of them takes, and a larger one is a malformed command. */
        if (count > 4 || !a.ok) return false;
        GLfloat values[4] = {};
        for (uint32_t i = 0; i < count; ++i) values[i] = a.f32();
        if (!a.ok) return false;
        if (op == LIGHTFV) glLightfv(target, pname, values);
        else glMaterialfv(target, pname, values);
        break;
    }
    case GEN_LISTS: put_u32(glGenLists(a.i32())); break;
    /* Protocol 11. The array pointers are offsets into the bound
     * GL_ARRAY_BUFFER, as VERTEX_ATTRIB_POINTER's are. */
    case ENABLE_CLIENT_STATE: glEnableClientState(a.u32()); break;
    case DISABLE_CLIENT_STATE: glDisableClientState(a.u32()); break;
    case VERTEX_POINTER:
    case COLOR_POINTER:
    case TEX_COORD_POINTER: {
        const GLint size = a.i32(); const GLenum type = a.u32(); const GLsizei stride = a.i32();
        const uintptr_t off = static_cast<uintptr_t>(a.u64());
        if (!a.ok) return false;
        void *p = reinterpret_cast<void*>(off);
        if (op == VERTEX_POINTER) glVertexPointer(size, type, stride, p);
        else if (op == COLOR_POINTER) glColorPointer(size, type, stride, p);
        else glTexCoordPointer(size, type, stride, p);
        break;
    }
    case CLIENT_ACTIVE_TEXTURE: glClientActiveTexture(a.u32()); break;
    /* { slot, op, n, n words, bytes }: a client-memory array, as a
     * native GL takes them -- a pointer into memory, which st/mesa uploads with its
     * stream uploader. The guest used to copy each one into a buffer of its
     * own with glBufferData before every draw, and a buffer the pending batch
     * already reads cannot be rewritten in place: every such upload became a
     * staged copy that split the render pass (one game: 142 a frame, 52 passes
     * for 47 draws). `op` is the pointer command (glVertexPointer's,
     * glVertexAttribPointer's, ...) and the words are its arguments before
     * the pointer, which becomes the bytes' address here; it runs with no
     * GL_ARRAY_BUFFER bound, and the guest rebinds its own afterwards. */
    case CLIENT_ARRAY: {
        const uint32_t slot = a.u32(), inner = a.u32(), n = a.u32();
        if (!a.ok || n > 8 || inner == CLIENT_ARRAY) return false;
        uint32_t words[8];
        for (uint32_t i = 0; i < n; ++i) words[i] = a.u32();
        uint32_t len = 0;
        const uint8_t* bytes = a.blob(len);
        if (!a.ok) return false;
        std::vector<uint8_t>& mem = client_arrays[slot];
        mem.assign(bytes, bytes + len);
        if (mem.empty()) mem.push_back(0);
        uint8_t body[8 * 4 + 8];
        std::memcpy(body, words, n * 4);
        const uint64_t at = reinterpret_cast<uintptr_t>(mem.data());
        std::memcpy(body + n * 4, &at, 8);
        glBindBuffer(GL_ARRAY_BUFFER, 0);
        Reader r{body, body + n * 4 + 8};
        return one(inner, r);
    }
    case ORTHO: {
        const GLdouble l = a.f64(), r = a.f64(), b = a.f64();
        const GLdouble t = a.f64(), n = a.f64(), f = a.f64();
        glOrtho(l, r, b, t, n, f); break;
    }
    case SCALEF: {
        const GLfloat x = a.f32(), y = a.f32(), z = a.f32();
        glScalef(x, y, z); break;
    }
    case LOAD_MATRIXF:
    case MULT_MATRIXF: {
        GLfloat m[16];
        for (int i = 0; i < 16; ++i) m[i] = a.f32();
        if (!a.ok) return false;
        if (op == LOAD_MATRIXF) glLoadMatrixf(m); else glMultMatrixf(m);
        break;
    }
    case COLOR4F: {
        const GLfloat r = a.f32(), g = a.f32(), b = a.f32(), al = a.f32();
        glColor4f(r, g, b, al); break;
    }
    case TEX_ENVI: {
        const GLenum target = a.u32(), pname = a.u32(); const GLint param = a.i32();
        glTexEnvi(target, pname, param); break;
    }
    case ALPHA_FUNC: {
        const GLenum func = a.u32(); const GLfloat ref = a.f32();
        glAlphaFunc(func, ref); break;
    }
    case PUSH_ATTRIB: glPushAttrib(a.u32()); break;
    case POP_ATTRIB: glPopAttrib(); break;
#include "gl44-generated-cases.inc"
#include "gl-compat-generated-cases.inc"
    default:
        /* Named, once. A command the host does not know aborts the whole
         * batch -- the guest discards every queued byte, including the
         * PRESENT at the end of the frame -- and the guest's own message can
         * only say that it happened, not what caused it. */
        {
            static bool said;
            if (!said) {
                said = true;
                fprintf(stderr, "gexgl: unknown opcode %u; the batch carrying "
                                "it is discarded\n", (unsigned)op);
            }
        }
        return false;
    }
    if (!a.ok) {
        static bool said;
        if (!said) {
            said = true;
            fprintf(stderr, "gexgl: opcode %u ran out of arguments; the batch "
                            "carrying it is discarded\n", (unsigned)op);
        }
    }
    return a.ok;
}
} // namespace

extern "C" {
/* Select the client API before gex_gl_init. `api` is 0 for GLES and 1 for a
 * desktop core profile; the version is what the context asks for, and st/mesa
 * returns the latest it can satisfy. */
EMSCRIPTEN_KEEPALIVE void gex_gl_set_api(int api, int major, int minor) {
    backend_profile = static_cast<int>(api == 1 ? API_OPENGL_CORE
                                                : API_OPENGLES2);
    backend_major = major > 0 ? major : (api == 1 ? 3 : 3);
    backend_minor = major > 0 ? minor : (api == 1 ? 3 : 0);
    /* And for every client that has not appeared yet. */
    device_profile = backend_profile;
    device_major = backend_major;
    device_minor = backend_minor;
}

EMSCRIPTEN_KEEPALIVE int gex_gl_init(int width, int height) {
    const int result = initialize_backend(width, height);
    if (result != 0) return result;
    // GLES has a usable default vertex-array object. A core desktop context's
    // VAO 0 cannot carry attribute state, so materialize the GLES default once
    // at context creation.
    glGenVertexArrays(1, &default_vao);
    glBindVertexArray(default_vao);
    return 0;
}

/* `client` names the guest process whose commands these are: its own GL
 * context, its own object names, its own windows. Callers that predate it --
 * the smoke and conformance harnesses, which are the only client there is --
 * pass nothing and get client zero. */
EMSCRIPTEN_KEEPALIVE int gex_gl_submit(const uint8_t* data, uint32_t len,
                                       uint32_t client) {
    use_client(client);
    if (gl_counts() && (++gl_client_counts[current_client].first % 256) == 0)
        gl_count_line("batch");
    reply.clear(); uint32_t at = 0;
    while (at < len) {
        if (len - at < 8) return -1;
        uint32_t op, size; std::memcpy(&op, data + at, 4); std::memcpy(&size, data + at + 4, 4);
        if (size < 8 || size > len - at) return -1;
        Reader args{data + at + 8, data + at + size};
        /* Every opcode has one canonical payload shape.  Reject both short
         * reads and trailing data: silently accepting a prefix makes version
         * skew indistinguishable from a valid command and lets the guest and
         * worker advance through the stream with different assumptions. */
        if (!one(op, args) || !args.ok || args.p != args.end) {
            /* Which opcode, and which of the three ways it went wrong. A bare
               -2 says a batch was rejected and the caller then latches, so the
               application stops dead with no indication of what it asked for
               -- and the opcode is the whole diagnosis. */
            /* Kept, not only printed: a page reads this back and says it
             * where the failure is visible, which stderr in a worker is
             * not. One rejected batch is a client that stops dead, and the
             * opcode is the whole diagnosis. */
            if (!last_reject_detailed) {
                last_reject_op = op;
                last_reject_why = !args.ok ? 1 : args.p != args.end ? 2 : 3;
                last_reject_payload = size - 8;
                last_reject_client = current_client;
            }
            last_reject_detailed = false;
            static bool said;
            if (!said) {
                said = true;
                std::fprintf(stderr,
                             "gexgl: batch rejected at opcode %u (%s) "
                             "payload=%u client=%u\n", op,
                             !args.ok ? "short read"
                             : args.p != args.end ? "trailing data"
                             : "unhandled opcode",
                             size - 8, current_client);
            }
            return -2;
        }
        gl_error_check(op);
        at += size;
    }
    return 0;
}
/* Set one host-side environment variable, as "NAME=VALUE".
 *
 * The driver's diagnostics are all getenv-gated (GEX_WGPU_DUMP_WGSL and the
 * rest), and in the browser getenv answers NULL for every one of them unless
 * the page sets them here, before gex_gl_init. The Mesa fork's gex-wasm
 * targets export it. */
EMSCRIPTEN_KEEPALIVE void gex_gl_setenv(const char *pair) {
    if (!pair) return;
    const char *equals = std::strchr(pair, '=');
    if (!equals || equals == pair) return;
    const std::string name(pair, (size_t)(equals - pair));
    setenv(name.c_str(), equals + 1, 1);
}
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_reply_len() { return reply.size(); }
EMSCRIPTEN_KEEPALIVE const uint8_t* gex_gl_reply_data() { return reply.data(); }
/* gexd3d-mesa.cpp, when the D3D device is built in. */
extern "C" __attribute__((weak)) void gex_d3d_display_tick();

/* Show one frame of every window that has one waiting.
 *
 * Not just the current one: a page frame shows every window, and a client
 * whose window is not the one that submitted last must not be left waiting
 * for a count that only moves when it is. */
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_display() {
    /* The D3D device's, when it is built in (gexd3d-mesa.cpp). */
    if (gex_d3d_display_tick) gex_d3d_display_tick();
    const uint32_t was_client = current_client;
    const uint32_t was_surface = current_surface;
    std::vector<uint32_t> ids;
    ids.push_back(was_client);
    for (const auto& entry : clients)
        if (entry.first != was_client) ids.push_back(entry.first);
    for (uint32_t id : ids) {
        use_client(id);
        /* A client with no GL context may still have frames: the D3D
         * device's (gex_gl_external_frame). */
        if (!mesa_context && surfaces.empty()) continue;
        std::vector<uint32_t> pending;
        for (const auto& entry : surfaces)
            if (display_pending(entry.second))
                pending.push_back(entry.first);
        for (uint32_t handle : pending) {
            use_surface(handle);
            display_backend();
        }
    }
    use_client(was_client);
    use_surface(was_surface);
    return displayed;
}

/* The windows the host is drawing, as the page reads them.
 *
 * One entry per attached surface across every client: the page pairs each
 * with the window it belongs to (a `wl_surface` id, or an X drawable) and
 * asks the renderer for that surface's own frame. A snapshot is taken when
 * the count is asked for, so the indices below are stable for one read. */
struct surface_ref {
    uint32_t client, handle, uid, kind, id, token, width, height;
};
std::vector<surface_ref> surface_list;

void collect_surfaces() {
    surface_list.clear();
    for (const auto& entry : surfaces)
        if (entry.second.kind || entry.second.id)
            surface_list.push_back({current_client, entry.first,
                                    entry.second.uid, entry.second.kind,
                                    entry.second.id, entry.second.token,
                                    entry.second.drawable.width,
                                    entry.second.drawable.height});
    /* The other clients' surfaces are in their saved state, and readable
     * there without swapping anything in. */
    for (const auto& client : clients) {
        if (client.first == current_client) continue;
        for (const auto& entry : client.second.surfaces)
            if (entry.second.kind || entry.second.id)
                surface_list.push_back({client.first, entry.first,
                                        entry.second.uid, entry.second.kind,
                                        entry.second.id, entry.second.token,
                                        entry.second.drawable.width,
                                        entry.second.drawable.height});
    }
}
/* How many windows the host is drawing, and what each one is. Call the count
 * first: it takes the snapshot the rest read. */
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_surface_count() {
    collect_surfaces();
    return (uint32_t)surface_list.size();
}
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_surface_kind(uint32_t i) {
    return i < surface_list.size() ? surface_list[i].kind : 0;
}
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_surface_id(uint32_t i) {
    return i < surface_list.size() ? surface_list[i].id : 0;
}
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_surface_token(uint32_t i) {
    return i < surface_list.size() ? surface_list[i].token : 0;
}
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_surface_client(uint32_t i) {
    return i < surface_list.size() ? surface_list[i].client : 0;
}
/* What the renderer keys this surface's frame by. */
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_surface_frame(uint32_t i) {
    return i < surface_list.size() ? surface_list[i].uid : 0;
}
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_surface_width(uint32_t i) {
    return i < surface_list.size() ? surface_list[i].width : 0;
}
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_surface_height(uint32_t i) {
    return i < surface_list.size() ? surface_list[i].height : 0;
}
/* Bumped whenever a surface is attached or one goes away, so a page can tell
 * a changed list from the same list read twice. */
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_surface_generation() {
    return attached_generation + surface_gone_generation;
}

/* This client is gone: its process exited, or the page dropped it.
 *
 * A client that exits says nothing -- it is not there to say it -- so the
 * page, which does notice, says it instead. Everything the client owned goes
 * back: its windows' colour and depth buffers, and the GL context that holds
 * its textures, its programs and its buffers. Without this a session that ran
 * two GL clients in turn leaked a context per client, and the names the dead
 * one had allocated were still in the maps the next one drew with. */
EMSCRIPTEN_KEEPALIVE void gex_gl_forget_client(uint32_t client) {
    const bool known = clients.find(client) != clients.end();
    if (!known && client != current_client) return;
    const uint32_t was = current_client;
    use_client(client);
    /* The context first, while the drawables it was built on still exist,
     * and then the surfaces with nothing to point at instead. Dropping the
     * current surface the usual way points the context at another one, and
     * with none left that is surface 0, made anew -- so `surfaces` was never
     * empty and this looped for ever in the compositor's Worker, which then
     * drew nothing and answered nothing (the second glxgears of a session,
     * whichever way it was started). */
    if (mesa_context) {
        st_destroy_context(mesa_context);
        mesa_context = nullptr;
    }
    sur = nullptr;
    while (!surfaces.empty()) drop_surface(surfaces.begin()->first, false);
    current_surface = 0;
    /* Back to whoever was running, unless that was this one. */
    use_client(was == client ? 0 : was);
    clients.erase(client);
}

/* A frame for one of a client's windows that this decoder did not draw:
 * the D3D device's (gexd3d-mesa.cpp), which shares this table of windows so
 * that its frames are shown, listed and forgotten as a GL client's are.
 *
 * `handle` names the drawable, from a range the client's EGL does not use;
 * `image` becomes its back buffer, held until the next frame replaces it. */
EMSCRIPTEN_KEEPALIVE void gex_gl_external_frame(uint32_t client, uint32_t handle,
                                                unsigned kind, unsigned id,
                                                unsigned token,
                                                struct pipe_resource *image,
                                                unsigned width, unsigned height) {
    const uint32_t was = current_client;
    use_client(client);
    gex_surface *s = surface_for(handle);
    if (s->kind != kind || s->id != id || s->token != token) {
        s->kind = kind;
        s->id = id;
        s->token = token;
        ++s->generation;
        attached_generation++;
    }
    s->external = true;
    pipe_resource_reference(&s->drawable.attachments[ST_ATTACHMENT_BACK_LEFT], image);
    s->drawable.width = width;
    s->drawable.height = height;
    ++produced;
    ++s->produced;
    use_client(was);
}

/* Why the last batch was refused: see `last_reject_op`. */
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_reject_op() { return last_reject_op; }
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_reject_why() { return last_reject_why; }
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_reject_payload() { return last_reject_payload; }
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_reject_client() { return last_reject_client; }

EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_attached_kind() { return attached_kind; }
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_attached_id() { return attached_id; }
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_attached_token() { return attached_token; }
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_attached_generation() { return attached_generation; }
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_presented() { return displayed; }
/* Surface `uid` (`gex_gl_surface_frame`) has a canvas of its own in the page,
 * with the id `gex-gl-<uid>`; `on` zero when it is gone. */
EMSCRIPTEN_KEEPALIVE void gex_gl_surface_canvas(uint32_t uid, uint32_t on) {
    set_window_canvas(uid, on != 0);
}
EMSCRIPTEN_KEEPALIVE uint32_t gex_gl_produced() { return produced; }
}
