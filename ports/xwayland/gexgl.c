/*
 * The host's GL frames for an X window, put into that window's own pixels.
 *
 * A GL client's frames are drawn by the page's GL device, not by the X
 * server. With a window manager of the compositor's own (rootless), each X
 * window is a surface of the page's and the frame is shown on it directly.
 * Rootful there is one surface, the X screen, under the session's own window
 * manager: a frame has to be in the window's pixels for the server's stacking
 * and clipping, and the manager's frames, to apply to it. So the page reads
 * the frame back and hands it here (hostx.js `xgl`), and this puts it into
 * the window as a PutImage from a client would.
 *
 * Done between two turns of the server's main loop (`gex_poll`), where no
 * request is half handled: never in a Wayland round trip's wait.
 */
#include <xwayland-config.h>

#include <emscripten.h>
#include <stdint.h>

#include "dix.h"
#include "gcstruct.h"
#include "scrnintstr.h"
#include "windowstr.h"

/* A frame waiting for its window: its pixels (BGRA, rows of 4 * w bytes, in
 * this module's memory), or 0. */
EM_JS(int, gexjs_xgl_take, (uint32_t *xid, int *w, int *h), {
    return Module.gexos.xglTake ? Module.gexos.xglTake(xid, w, h) : 0;
});
/* What became of it: 1 put, 0 the window is not viewable, -1 no such window. */
EM_JS(void, gexjs_xgl_done, (int result), { Module.gexos.xglDone?.(result); });
/* The window whose frames are wanted, to say whether it is viewable. */
EM_JS(uint32_t, gexjs_xgl_watch, (void), { return Module.gexos.xglWatch ? Module.gexos.xglWatch() : 0; });
EM_JS(void, gexjs_xgl_viewable, (uint32_t xid, int viewable), { Module.gexos.xglViewable?.(xid, viewable); });

static WindowPtr
window_of(uint32_t xid)
{
    WindowPtr win = NULL;
    if (!xid || dixLookupWindow(&win, xid, serverClient, DixWriteAccess) != Success)
        return NULL;
    return win;
}

static int
put(WindowPtr win, int w, int h, const void *bits)
{
    DrawablePtr d = &win->drawable;
    if (d->bitsPerPixel != 32)
        return -1;
    if (w <= 0 || h <= 0)
        return 1;
    GCPtr gc = GetScratchGC(d->depth, d->pScreen);
    if (!gc)
        return -1;
    ValidateGC(d, gc);
    /* The frame at its own size: a window smaller than it (resized since
     * the frame was drawn) is cut by the window's clip, as is any part
     * another window covers. */
    (*gc->ops->PutImage)(d, gc, d->depth, 0, 0, w, h, 0, ZPixmap, (char *) bits);
    FreeScratchGC(gc);
    return 1;
}

/* Called by `gex_poll` between turns of the main loop. Returns whether a
 * frame was put (the loop then goes round, and the damage is posted). */
int
gex_xgl_service(void)
{
    uint32_t watch = gexjs_xgl_watch();
    if (watch) {
        WindowPtr win = window_of(watch);
        gexjs_xgl_viewable(watch, win ? (win->viewable ? 1 : 0) : -1);
    }
    uint32_t xid = 0;
    int w = 0, h = 0;
    int bits = gexjs_xgl_take(&xid, &w, &h);
    if (!bits)
        return 0;
    WindowPtr win = window_of(xid);
    int r = !win ? -1 : !win->viewable ? 0 : put(win, w, h, (const void *) (uintptr_t) bits);
    gexjs_xgl_done(r);
    return r > 0;
}
