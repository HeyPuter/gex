/* GLX, over the EGL in this same object.
 *
 * Included by gexgl.c and exported from libGL.so.1, which is another copy of
 * the one object -- the same arrangement libEGL.so.1 already uses. Nothing
 * here talks to the host directly: every entry point is a thin translation
 * into the EGL above, which is where the transport lives.
 *
 * The point of it is that GLX, not EGL, is how a traditional Linux program
 * asks X for a GL context, and this project had no libGL at all, so that
 * whole class could not run. glmark2 is the immediate reason -- thirty scenes
 * of shader-based rendering, and its GLX flavour is what Debian ships.
 *
 * What makes this small is that the EGL below already answers with one config
 * and one context, so the GLX objects can be constants: there is exactly one
 * of each to hand out, and the interesting work -- learning the window's size
 * and the server's visual -- was already written for eglCreateWindowSurface
 * and eglGetConfigAttrib.
 */

/* Xlib's XVisualInfo, declared rather than included: this library must build
 * without X11 headers, and the layout is stable ABI that every Xlib client
 * already depends on. */
struct gexgl_visual_info {
    void *visual;
    unsigned long visualid;
    int screen;
    int depth;
    int class_;
    unsigned long red_mask;
    unsigned long green_mask;
    unsigned long blue_mask;
    int colormap_size;
    int bits_per_rgb;
};

/* The configs on offer.
 *
 * The backend has one framebuffer shape -- 8/8/8/8 with a 24/8 depth-stencil
 * -- but a GLX driver that advertises exactly one config is unusual, and a
 * program that scores configs against what it asked for has nothing to
 * choose. These four differ in the two components an application actually
 * varies: whether it wants alpha, and whether it wants stencil. All four are
 * served by the same buffer, so offering them costs nothing and asks nothing
 * of the backend.
 *
 * Each entry's address is its GLXFBConfig handle. */
struct gexgl_glx_config {
    int alpha;
    int stencil;
};
static const struct gexgl_glx_config gexgl_glx_configs[] = {
    {8, 8}, {8, 0}, {0, 8}, {0, 0},
};
#define GEXGL_GLX_CONFIG_COUNT \
    ((int)(sizeof(gexgl_glx_configs) / sizeof(gexgl_glx_configs[0])))
#define GEXGL_GLX_CONFIG ((void *)&gexgl_glx_configs[0])
static const int gexgl_glx_context_token = 0;
#define GEXGL_GLX_CONTEXT ((void *)&gexgl_glx_context_token)

static void *glx_current_display;
static unsigned long glx_current_drawable;

static void gexgl_glx_ensure_egl(void *dpy);

/* TrueColor at depth 24, which is what the config this library advertises
 * describes and what Xwayland gives a client that asks for nothing. */
static int gexgl_fill_visual_info(void *display, int screen,
                                  struct gexgl_visual_info *out) {
    /* Records the display and the platform, which is what lets the X helpers
       shared with EGL use the caller's connection. */
    gexgl_glx_ensure_egl(display);
    void *xlib = gexgl_x11_symbol_source();
    if (!xlib || !display) return 0;
    int (*match)(void *, int, int, int, struct gexgl_visual_info *) =
        dlsym(xlib, "XMatchVisualInfo");
    if (!match) return 0;
    if (match(display, screen, 24, 4 /* TrueColor */, out)) return 1;
    return match(display, screen, 32, 4, out) != 0;
}

int glXQueryVersion(void *dpy, int *major, int *minor) {
    (void)dpy;
    if (major) *major = 1;
    if (minor) *minor = 4;
    return 1;
}

int glXQueryExtension(void *dpy, int *error_base, int *event_base) {
    (void)dpy;
    if (error_base) *error_base = 0;
    if (event_base) *event_base = 0;
    return 1;
}

const char *glXQueryExtensionsString(void *dpy, int screen) {
    (void)dpy; (void)screen;
    /* What this library actually implements, named the way a program looks
       for it. Toolkits check this string before using an entry point at all:
       SDL asks for the create-context extensions before it will make
       anything but a legacy context, and for the swap-control ones before it
       will honour a swap interval. Claiming only the first two left three
       implemented entry points unreachable.
       Nothing here claims sRGB framebuffers or float pixel formats: those
       would have to be true of the backend's buffer, and are not. */
    return "GLX_ARB_get_proc_address "
           "GLX_ARB_create_context "
           "GLX_ARB_create_context_profile "
           "GLX_ARB_create_context_robustness "
           "GLX_EXT_create_context_es2_profile "
           "GLX_ARB_multisample "
           "GLX_EXT_visual_info "
           "GLX_EXT_visual_rating "
           "GLX_EXT_swap_control "
           "GLX_MESA_swap_control "
           "GLX_SGI_swap_control "
           "GLX_SGIX_fbconfig "
           "GLX_SGI_make_current_read";
}

const char *glXGetClientString(void *dpy, int name) { (void)dpy; (void)name; return "gex"; }
const char *glXQueryServerString(void *dpy, int screen, int name) {
    (void)dpy; (void)screen; (void)name;
    return "gex";
}

/* The attribute lists are read only far enough to refuse what cannot be
 * given. There is one config; a program asking for something else should be
 * told so rather than handed a config that does not match. */
void *glXChooseVisual(void *dpy, int screen, int *attribs) {
    for (int *a = attribs; a && *a != 0 /* None */; ) {
        const int name = *a++;
        /* GLX_USE_GL, GLX_RGBA, GLX_DOUBLEBUFFER and GLX_STEREO carry no
           value; everything else takes one. */
        if (name == 1 || name == 4 || name == 5 || name == 6) continue;
        ++a;
    }
    struct gexgl_visual_info *info = malloc(sizeof(*info));
    if (!info) return NULL;
    memset(info, 0, sizeof(*info));
    if (!gexgl_fill_visual_info(dpy, screen, info)) {
        free(info);
        return NULL;
    }
    return info;
}

/* GLX_RGBA_BIT and GLX_WINDOW_BIT are both 1, which is why the two masks
 * below look identical. */
static int gexgl_glx_config_attrib(const struct gexgl_glx_config *config,
                                   int attribute, int *value) {
    const int alpha = config ? config->alpha : 8;
    const int stencil = config ? config->stencil : 8;
    switch (attribute) {
    case 2:  *value = 24 + alpha; return 1;   /* GLX_BUFFER_SIZE */
    case 3:  *value = 0;  return 1;   /* GLX_LEVEL */
    case 5:  *value = 1;  return 1;   /* GLX_DOUBLEBUFFER */
    case 6:  *value = 0;  return 1;   /* GLX_STEREO */
    case 7:  *value = 0;  return 1;   /* GLX_AUX_BUFFERS */
    case 8: case 9: case 10: *value = 8; return 1;  /* R/G/B size */
    case 11: *value = alpha; return 1;              /* GLX_ALPHA_SIZE */
    case 12: *value = 24; return 1;   /* GLX_DEPTH_SIZE */
    case 13: *value = stencil; return 1; /* GLX_STENCIL_SIZE */
    case 14: case 15: case 16: case 17: *value = 0; return 1; /* accum */
    case 0x20: *value = 0x8000; return 1;  /* GLX_CONFIG_CAVEAT: GLX_NONE */
    case 0x22: *value = 0x8002; return 1;  /* X_VISUAL_TYPE: TRUE_COLOR */
    case 0x8010: *value = 1; return 1;     /* GLX_DRAWABLE_TYPE: WINDOW_BIT */
    case 0x8011: *value = 1; return 1;     /* GLX_RENDER_TYPE: RGBA_BIT */
    case 0x8012: *value = 1; return 1;     /* GLX_X_RENDERABLE */
    case 0x8013:                           /* GLX_FBCONFIG_ID */
        *value = config ? (int)(config - gexgl_glx_configs) + 1 : 1;
        return 1;
    /* The X visual this config can make a window on. Zero says "none", which
       is what a config unusable for windows reports -- so it was both wrong
       and the kind of wrong a config-scoring program is entitled to reject
       outright. */
    case 0x800B: *value = gexgl_x11_default_visual(); return 1;
    case 100000: case 100001: *value = 0; return 1; /* sample buffers/samples */
    default: *value = 0; return 1;
    }
}

int glXGetConfig(void *dpy, void *visual, int attribute, int *value) {
    (void)dpy; (void)visual;
    if (!value) return 2; /* GLX_BAD_VALUE */
    /* GLX_USE_GL, which glXGetConfig answers and glXGetFBConfigAttrib does
       not. */
    if (attribute == 1) { *value = 1; return 0; }
    if (attribute == 4) { *value = 1; return 0; } /* GLX_RGBA */
    gexgl_glx_config_attrib(&gexgl_glx_configs[0], attribute, value);
    return 0;
}

void **glXGetFBConfigs(void *dpy, int screen, int *count) {
    (void)screen;
    gexgl_glx_ensure_egl(dpy);
    void **configs = malloc(sizeof(*configs) * GEXGL_GLX_CONFIG_COUNT);
    if (!configs) { if (count) *count = 0; return NULL; }
    for (int i = 0; i < GEXGL_GLX_CONFIG_COUNT; ++i)
        configs[i] = (void *)&gexgl_glx_configs[i];
    if (count) *count = GEXGL_GLX_CONFIG_COUNT;
    return configs;
}

void **glXChooseFBConfig(void *dpy, int screen, const int *attribs,
                         int *count) {
    (void)attribs;
    return glXGetFBConfigs(dpy, screen, count);
}

int glXGetFBConfigAttrib(void *dpy, void *config, int attribute, int *value) {
    (void)dpy;
    if (!value) return 2;
    gexgl_glx_config_attrib(config, attribute, value);
    return 0;
}

void *glXGetVisualFromFBConfig(void *dpy, void *config) {
    (void)config;
    struct gexgl_visual_info *info = malloc(sizeof(*info));
    if (!info) return NULL;
    memset(info, 0, sizeof(*info));
    /* Screen 0: this library serves one, and the drawable it is handed later
       settles the size. */
    if (!gexgl_fill_visual_info(dpy, 0, info)) {
        free(info);
        return NULL;
    }
    return info;
}

/* Bring the EGL underneath up, which GLX never does for itself.
 *
 * A GLX program calls no EGL at all, so `egl_ready` stayed false and
 * eglMakeCurrent refused every context -- glxinfo got as far as
 * "glXMakeCurrent failed" with the visual, the config and the context all
 * correct. Initialising here is the whole of the bridge.
 *
 * The platform is stated rather than guessed: a GLX display is an Xlib
 * Display by definition, which lets the visual and geometry helpers use the
 * caller's own connection instead of opening one, and removes the ambiguity
 * eglGetDisplay has to live with. */
static void gexgl_glx_ensure_egl(void *dpy) {
    void *display = eglGetDisplay(dpy);
    egl_native_platform = 0x31D5u; /* EGL_PLATFORM_X11_KHR */
    /* Compatibility, which is what a GLX context is unless the application
       says otherwise with glXCreateContextAttribsARB. */
    if (!host_context_made) egl_context_profile = 1;
    eglInitialize(display, NULL, NULL);
    /* Desktop GL, always: a program reaching for GLX is asking for it by
       name, and the EGL below selects its client API from eglBindAPI. */
    eglBindAPI(0x30A2u /* EGL_OPENGL_API */);
}

static void *gexgl_glx_make_context(void *dpy) {
    gexgl_glx_ensure_egl(dpy);
    return GEXGL_GLX_CONTEXT;
}

void *glXCreateContext(void *dpy, void *visual, void *share, int direct) {
    (void)visual; (void)share; (void)direct;
    return gexgl_glx_make_context(dpy);
}

void *glXCreateNewContext(void *dpy, void *config, int render_type,
                          void *share, int direct) {
    (void)config; (void)render_type; (void)share; (void)direct;
    return gexgl_glx_make_context(dpy);
}

void *glXCreateContextAttribsARB(void *dpy, void *config, void *share,
                                 int direct, const int *attribs) {
    (void)config; (void)share; (void)direct;
    gexgl_glx_ensure_egl(dpy);
    /* Unlike the legacy entry points, this one is told what to make. The
       version is forwarded the same way EGL's is -- the host still reports
       what it actually got through GL_VERSION -- and an explicit core request
       overrides the compatibility default above. */
    int wants_es = 0;
    for (const int *a = attribs; a && *a != 0 && !host_context_made; a += 2) {
        switch (a[0]) {
        case 0x2091: egl_context_major = a[1]; break; /* MAJOR_VERSION_ARB */
        case 0x2092: egl_context_minor = a[1]; break; /* MINOR_VERSION_ARB */
        case 0x9126: /* GLX_CONTEXT_PROFILE_MASK_ARB */
            /* 0x1 core, 0x2 compatibility, 0x4 OpenGL ES -- the last from
               GLX_EXT_create_context_es2_profile, and an application that
               asks for it is asking for ES however it got here.
               SuperTuxKart asks GLX for ES 3 exactly this way, and the
               request used to be rewritten to ES 2 by a preloaded library
               because the driver underneath refused it. */
            wants_es = (a[1] & 0x4) != 0;
            egl_context_profile = (a[1] & 0x2) ? 1 : 0;
            break;
        default:
            /* Flags, reset notification, release behaviour: read past them.
               A GLX driver that refuses a context because it was told
               something it does not act on is worse than one that ignores
               it, and the host reports what the context really is. */
            break;
        }
    }
    /* After the attributes, not before: `gexgl_glx_ensure_egl` binds desktop
       GL because that is what reaching for GLX usually means, and only the
       profile mask can say otherwise. */
    if (wants_es) eglBindAPI(0x30A0u /* EGL_OPENGL_ES_API */);
    return GEXGL_GLX_CONTEXT;
}

/* Destroying the context really destroys it, which is what makes an odd
 * initialization work.
 *
 * A toolkit commonly makes a throwaway context to read GL_VERSION and the
 * extension strings, destroys it, and only then creates the one it wants --
 * SDL does exactly this. The host context used to be made once and latched,
 * so the second request's profile and version were quietly ignored and the
 * application ran on whatever the throwaway had asked for. Letting the host
 * drop it means the next creation is a real creation. */
void glXDestroyContext(void *dpy, void *ctx) {
    (void)dpy; (void)ctx;
    eglDestroyContext(NULL, ctx);
}

int glXMakeCurrent(void *dpy, unsigned long drawable, void *ctx) {
    gexgl_glx_ensure_egl(dpy);
    if (!ctx) {
        eglMakeCurrent(dpy, NULL, NULL, NULL);
        glx_current_display = NULL;
        glx_current_drawable = 0;
        return 1;
    }
    /* The drawable is an X window id, which is exactly what
       eglCreateWindowSurface takes here -- it reads the window's geometry and
       resizes the host surface to match. */
    void *config = NULL;
    void *surface = eglCreateWindowSurface(dpy, config,
                                           (void *)(uintptr_t)drawable, NULL);
    void *context = egl_current_context ? egl_current_context
                                        : eglCreateContext(dpy, config, NULL,
                                                           NULL);
    if (!eglMakeCurrent(dpy, surface, surface, context)) return 0;
    glx_current_display = dpy;
    glx_current_drawable = drawable;
    return 1;
}

int glXMakeContextCurrent(void *dpy, unsigned long draw, unsigned long read,
                          void *ctx) {
    (void)read;
    return glXMakeCurrent(dpy, draw, ctx);
}

void glXSwapBuffers(void *dpy, unsigned long drawable) {
    /* The drawable named here, not whatever is current: an application with
       two windows swaps each in turn, and GLX has no notion of "the current
       one" for this call. The surface for that window is the one this
       library already made for it. */
    struct gexgl_egl_surface *s =
        egl_surface_for_native((void *)(uintptr_t)drawable);
    eglSwapBuffers(dpy, s ? (void *)(uintptr_t)s->handle : egl_current_draw);
}

void *glXGetCurrentContext(void) {
    return egl_current_context ? GEXGL_GLX_CONTEXT : NULL;
}
void *glXGetCurrentDisplay(void) { return glx_current_display; }
unsigned long glXGetCurrentDrawable(void) { return glx_current_drawable; }
unsigned long glXGetCurrentReadDrawable(void) { return glx_current_drawable; }

int glXIsDirect(void *dpy, void *ctx) { (void)dpy; (void)ctx; return 1; }

/* GLX_WIDTH / GLX_HEIGHT, which a program asks for after a resize. The
   drawable is an X window id, so this is the same geometry query
   eglCreateWindowSurface makes. */
void glXQueryDrawable(void *dpy, unsigned long drawable, int attribute,
                      unsigned *value) {
    (void)dpy;
    if (!value) return;
    int width = 0, height = 0;
    if (!gexgl_x11_window_size((void *)(uintptr_t)drawable, &width, &height))
        return;
    switch (attribute) {
    case 0x801D: *value = (unsigned)width; break;  /* GLX_WIDTH */
    case 0x801E: *value = (unsigned)height; break; /* GLX_HEIGHT */
    default: break;
    }
}

void glXWaitGL(void) { glFinish(); }
void glXWaitX(void) { }

void glXSwapIntervalEXT(void *dpy, unsigned long drawable, int interval) {
    (void)dpy; (void)drawable;
    eglSwapInterval(dpy, interval);
}
int glXSwapIntervalMESA(unsigned interval) {
    eglSwapInterval(NULL, (int)interval);
    return 0;
}
int glXSwapIntervalSGI(int interval) {
    eglSwapInterval(NULL, interval);
    return 0;
}

void *glXGetProcAddressARB(const unsigned char *name) {
    return eglGetProcAddress((const char *)name);
}
void *glXGetProcAddress(const unsigned char *name) {
    return glXGetProcAddressARB(name);
}
