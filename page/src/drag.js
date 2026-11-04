// A drop of host files on a guest window (app.js `drop`), as the guest's
// Wayland compositor sees it (hosts/browser/src/wl.rs): once the files are
// copied, a drag of their text/uri-list enters the window under the point,
// moves there, and is dropped if the window's program accepts it. Each guest
// process's compositor says through its block (wl-bridge.mjs `waitDrag`)
// whether its client accepts, and at the drop whether it took it.
//
// The drag starts only once the files are in the guest: a program may read
// the list as the drag enters, to decide (Wine does), and a browser names
// the files only at the drop.

/** How long the program gets to accept the drag once it entered. */
const ACCEPT_MS = 2000;
/** How long it gets to answer the drop. */
const ANSWER_MS = 2000;

/** `paths` as a text/uri-list: `file://` URIs, CRLF after each, every byte
 *  outside RFC 3986's unreserved set and `/` percent-encoded (as the native
 *  compositor's `uri_list`). */
export function uriList(paths) {
  let out = '';
  for (const p of paths) {
    out += 'file://';
    for (const b of new TextEncoder().encode(p)) {
      const c = String.fromCharCode(b);
      out += /[A-Za-z0-9\-._~/]/.test(c) ? c : `%${b.toString(16).toUpperCase().padStart(2, '0')}`;
    }
    out += '\r\n';
  }
  return out;
}

export class PageDrag {
  #channel;
  #wl;
  #serial = 0;

  /** `channel`: the page's Wayland channel; `wl`: wl-bridge.mjs. */
  constructor(channel, wl) {
    this.#channel = channel;
    this.#wl = wl;
  }

  /** Files at `paths` (guest paths) dropped on window `w` (`windowAt`'s:
   *  {block, client, surface, x, y}). Resolves whether its program took
   *  them; a drag it does not accept there in time is left, and false. */
  async drop(w, paths) {
    const wl = this.#wl;
    if (!w || w.block === undefined || !paths.length) return false;
    if (!wl.setDragData(this.#channel, uriList(paths))) return false;
    const serial = (this.#serial = ((this.#serial + 1) & 0x0fffffff) || 1);
    const send = (kind, a = 0, b = 0, c = 0) => wl.sendEvent(this.#channel, w.block, kind, a, b, c, 0, w.client, w.surface);
    send(wl.EV_DRAG_ENTER, w.x, w.y, serial);
    // And a motion: a program may decide only on one.
    send(wl.EV_DRAG_MOTION, w.x, w.y, Math.round(performance.now()) | 0);
    const said = await wl.waitDrag(this.#channel, w.block, serial, (f) => (f & (wl.DRAG_ACCEPTS | wl.DRAG_ANSWERED)) !== 0, ACCEPT_MS);
    if (said === null || !(said & wl.DRAG_ACCEPTS)) {
      send(wl.EV_DRAG_LEAVE);
      return false;
    }
    send(wl.EV_DROP, serial);
    const r = await wl.waitDrag(this.#channel, w.block, serial, (f) => (f & wl.DRAG_ANSWERED) !== 0, ANSWER_MS);
    return r !== null && (r & wl.DRAG_TAKEN) !== 0;
  }
}
