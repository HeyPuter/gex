// The desktop-layer check (windows/desktop-layer*.test.js): two rt-wl programs,
// the desktop -- the whole 640x480 output in gex_window_placement_v1's
// desktop layer (3) -- and a 200x100 window at 100,100 in the normal layer,
// sent to the bottom of its layer after its first frame. The window must show
// over the desktop, the desktop around it, and a press over each must reach
// its own program. Then a third, at 300,300, grabs the pointer (version 4,
// a Win32 menu's grab): a press over the desktop reaches it instead, outside
// it, until it is gone. `where`: the output's rectangle on the page.

const DESK = [0x20, 0x40, 0x60];
const WIN = [0x30, 0x60, 0xc0];

export async function desktopLayer(t, sys, out, where) {
  const { check, timeout, until, lines, input } = t;
  const near = (px, want) => want.every((w, k) => Math.abs(px[k] - w) <= 8);
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin', ...out.env };
  const desk = sys.spawn(['rt-wl', 'desk', 'size', '640', '480', 'colour', '204060', 'place', '0', '0', '3'], { env });
  const deskSaid = lines(desk);
  await until(() => deskSaid.some((l) => l.startsWith('drawn')), 'the desktop drawn', 60000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(deskSaid)} ${JSON.stringify(sys.state.console.slice(-400))}`); });
  const win = sys.spawn(['rt-wl', 'win', 'place', '100', '100', '0', 'bottom'], { env });
  const winSaid = lines(win);
  await until(() => winSaid.some((l) => l.startsWith('drawn')), 'the window drawn', 60000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(winSaid)}`); });
  let px = null;
  await until(async () => {
    px = await t.pixels({ x: where.left, y: where.top, width: 640, height: 480 });
    const at = (x, y) => [...px.data.slice((y * 640 + x) * 4, (y * 640 + x) * 4 + 3)];
    return near(at(150, 150), WIN) && near(at(500, 400), DESK);
  }, 'both on the screen', 20000).catch(() => {});
  const at = (x, y) => [...px.data.slice((y * 640 + x) * 4, (y * 640 + x) * 4 + 3)];
  check(`the window, sent to the bottom, is over the desktop (${at(150, 150)})`, near(at(150, 150), WIN));
  check(`the desktop shows around it (${at(500, 400)})`, near(at(500, 400), DESK));
  if (!near(at(150, 150), WIN) || !near(at(500, 400), DESK)) await t.screenshot('desktop-layer');
  const press = async (x, y, said, want, what) => {
    const before = said.length;
    const cx = where.left + x, cy = where.top + y;
    await input({ method: 'Input.dispatchMouseEvent', type: 'mouseMoved', x: cx, y: cy, button: 'none', buttons: 0 });
    await new Promise((r) => setTimeout(r, 300));
    await input(
      { method: 'Input.dispatchMouseEvent', type: 'mousePressed', x: cx, y: cy, button: 'left', buttons: 1, clickCount: 1 },
      { method: 'Input.dispatchMouseEvent', type: 'mouseReleased', x: cx, y: cy, button: 'left', buttons: 0, clickCount: 1 },
    );
    const m = await until(() => said.slice(before).map((l) => /^pressed (-?\d+) (-?\d+)$/.exec(l)).find(Boolean), what, 10000).catch(() => null);
    const got = m && [Number(m[1]), Number(m[2])];
    check(`${what} (${got ?? 'no press'}, want ${want})`, !!got && Math.abs(got[0] - want[0]) <= 1 && Math.abs(got[1] - want[1]) <= 1);
  };
  await press(500, 400, deskSaid, [500, 400], 'a press over the bare desktop reaches the desktop');
  await press(150, 150, winSaid, [50, 50], 'a press over the window reaches the window');
  const grab = sys.spawn(['rt-wl', 'grab', 'place', '300', '300', '0', 'grab'], { env });
  const grabSaid = lines(grab);
  await until(() => grabSaid.some((l) => l.startsWith('drawn')), 'the grabbing window drawn', 60000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(grabSaid)}`); });
  await press(500, 400, grabSaid, [200, 100], 'under a grab, a press over the desktop reaches the grabbing window');
  await press(150, 150, grabSaid, [-150, -150], 'and one over the other window');
  grab.kill('SIGKILL');
  await new Promise((r) => setTimeout(r, 1000));
  await press(500, 400, deskSaid, [500, 400], 'the grab gone with its window, a press over the desktop reaches the desktop');
  desk.kill('SIGKILL');
  win.kill('SIGKILL');
  await sys.shutdown();
}
