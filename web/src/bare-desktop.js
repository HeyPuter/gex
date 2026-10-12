// The page with no compositor: a program's windows drawn straight into the
// page, one canvas each, laid out by the browser.
//
// gex-eject's `"compositor": false` (docs/eject.md, "Features"). The scene
// compositor (`compositor-worker.js`) draws every surface into one WebGPU
// canvas, with a frame and a title bar around each toplevel, a cursor sprite
// and its own hit testing. A program that draws its own frames -- Wine does,
// a Windows window's caption and border are the window's -- needs none of
// that: this puts each surface's `DomDesktop` canvas in a positioned element
// and lets the browser composite them. No WebGPU device, no worker, no scene,
// no decorations; the popups (menus, the Ribbon's drop-downs, tooltips) sit
// above their parents where the client placed them.
//
// It is a `DomDesktop` underneath (dom-compositor.js): the same surface arena,
// input ring and protocol in each guest process. What this file adds is the
// layout a page would otherwise do, and the `describe()` a harness reads from
// the scene compositor (titles, the surface list, a content hash).

import { ROLE_TOPLEVEL, ROLE_POPUP, ROLE_CURSOR } from './dom-compositor.js';

/** Lay `desktop`'s windows out in `mount` (a positioned element). */
export function bareDesktop(desktop, mount) {
  const placed = new Map(); // key -> { x, y }
  let z = 1;
  let cascade = 0;
  const keyOfParent = (win) => `${win.token}:${win.client}:${win.parent}`;
  const at = (win) => {
    if (win.role === ROLE_TOPLEVEL || !win.parent) return placed.get(win.key) ?? { x: 0, y: 0 };
    const p = placed.get(keyOfParent(win)) ?? { x: 0, y: 0 };
    return { x: p.x + (win.relX | 0), y: p.y + (win.relY | 0) };
  };
  const move = (win) => {
    const p = win.role === ROLE_TOPLEVEL ? placed.get(win.key) : at(win);
    placed.set(win.key, p);
    win.canvas.style.left = `${p.x}px`;
    win.canvas.style.top = `${p.y}px`;
    // Children follow: a popup's place is its parent's plus its offset.
    for (const w of desktop.windows()) if (w.role !== ROLE_TOPLEVEL && w.parent && keyOfParent(w) === win.key) move(w);
  };
  const size = (win) => {
    win.canvas.style.width = `${win.canvas.width}px`;
    win.canvas.style.height = `${win.canvas.height}px`;
  };
  desktop.onwindow = (win) => {
    if (win.role === ROLE_CURSOR) return;
    const c = win.canvas;
    c.style.position = 'absolute';
    size(win);
    if (win.role === ROLE_TOPLEVEL) {
      // The first window at the corner, the next ones cascaded: a toplevel
      // has no position in Wayland, and Wine's own frame is the decoration.
      const step = 28;
      const n = cascade++;
      const roomX = Math.max(1, mount.clientWidth - win.width);
      const roomY = Math.max(1, mount.clientHeight - win.height);
      placed.set(win.key, { x: (n * step) % roomX, y: (n * step) % roomY });
      c.style.zIndex = String(z++);
      c.addEventListener('pointerdown', () => { c.style.zIndex = String(z++); });
      win.place(placed.get(win.key).x, placed.get(win.key).y);
    } else {
      // Popups and subsurfaces above every toplevel.
      c.style.zIndex = String(1000000 + z++);
    }
    win.onplace = () => move(win);
    win.onresized = () => size(win);
    win.onclosed = () => { c.remove(); placed.delete(win.key); };
    mount.append(c);
    move(win);
  };

  /** What the scene compositor's `describe()` says, as far as a harness reads
   *  it: the toplevels' titles, every surface, and a hash of what is shown. */
  desktop.describe = async () => {
    const wins = desktop.windows().filter((w) => w.role !== ROLE_CURSOR);
    const tops = wins.filter((w) => w.role === ROLE_TOPLEVEL);
    let hash = 2166136261;
    let lit = 0;
    for (const w of [...wins].sort((a, b) => Number(a.canvas.style.zIndex) - Number(b.canvas.style.zIndex))) {
      if (w.canvas.width < 2 || w.canvas.height < 2 || w.external) continue;
      let px;
      try {
        px = w.canvas.getContext('2d').getImageData(0, 0, w.canvas.width, w.canvas.height).data;
      } catch {
        continue;
      }
      for (let i = 0; i < px.length; i += 16) {
        const v = Math.max(px[i], px[i + 1], px[i + 2]);
        if (v > 12) lit++;
        hash = ((hash ^ v) * 16777619) >>> 0;
      }
    }
    return {
      frames: tops.reduce((a, w) => a + w.paints, 0),
      windows: tops.length,
      surfaces: wins.length,
      list: wins.map((w) => {
        const p = at(w);
        return `${w.key} role=${w.role} ${w.width}x${w.height}@${p.x},${p.y} serial=${w.serial} "${w.title ?? ''}"`;
      }),
      titles: tops.map((w) => w.title || '(untitled)'),
      popups: wins.filter((w) => w.role === ROLE_POPUP).length,
      lit,
      hash,
      output: `${mount.clientWidth}x${mount.clientHeight}`,
      compositor: 'none',
    };
  };
  return desktop;
}
