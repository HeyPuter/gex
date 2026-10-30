// Gamepads: the page's controllers as /dev/input/eventN, read the way a
// program without udev reads them (probe/rt-joy.c). The controllers are
// made up here: a headless browser has none, and the Gamepad API cannot be
// driven from outside.
import { System } from '../src/index.js';
import { Filesystem } from '../src/fs.js';
import { Gamepads } from '../src/input.js';
import { main, check, eq, timeout, until, lines, testDebug } from './lib.js';

const button = (pressed = false, value = pressed ? 1 : 0) => ({ pressed, value });
function pad(index, id, mapping, nb, na) {
  return { index, id, mapping, connected: true, buttons: Array.from({ length: nb }, () => button()), axes: new Array(na).fill(0) };
}

main(async () => {
  const sys = new System({ baseUrl: '/', debug: testDebug() });
  const fs = new Filesystem(sys);
  fs.addTar('/__rt/bb.tar.zst');
  const list = [];
  const gp = new Gamepads(sys, { source: () => list });
  const said = [];
  gp.addEventListener('connect', (e) => said.push(`connect ${e.slot}`));
  gp.addEventListener('disconnect', (e) => said.push(`disconnect ${e.slot}`));
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin', ...gp.env };
  eq('env for SDL', gp.env, { SDL_JOYSTICK_DISABLE_UDEV: '1' });
  const ls = async () => (await sys.run(['rt-joy', 'list'], { env })).stdout.trim().split('\n');
  const none = await ls();
  eq('no controller, no node', none[0], 'none');

  // A standard pad: an Xbox 360 pad as xpad has it.
  const xbox = pad(0, 'Xbox 360 Controller (XInput STANDARD GAMEPAD)', 'standard', 17, 4);
  list[0] = xbox;
  await until(() => said.includes('connect 0'), 'the connect');
  const one = await ls();
  eq('the standard pad', one[0], 'event0 Microsoft X-Box 360 pad 045e:028e keys 11 abs 8 x -32768..32767 down 0 ev 0b');
  check('the directory moved', Number(one[1].split(' ')[1]) > 0 && one[1] !== none[1], `${none[1]} then ${one[1]}`);
  eq('pads', gp.pads, [{ slot: 0, id: xbox.id, mapping: 'standard' }]);

  const r = sys.spawn(['rt-joy', 'read', '0'], { env });
  const out = lines(r);
  await until(() => out.includes('ready'), 'the reader');
  await new Promise((res) => setTimeout(res, 200));
  xbox.buttons[0] = button(true);
  await until(() => out.includes('key 304 1'), 'A down', 5000).catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(out)}`); });
  eq('A down, then a report', out.slice(out.indexOf('key 304 1'), out.indexOf('key 304 1') + 2), ['key 304 1', 'syn']);
  xbox.axes[0] = 0.5;
  xbox.buttons[7] = button(true, 1);
  xbox.buttons[12] = button(true);
  await until(() => out.includes('abs 0 16384') && out.includes('abs 5 255') && out.includes('abs 17 -1'), 'a stick, a trigger and the d-pad', 5000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(out)}`); });
  check('a stick, a trigger and the d-pad', true);
  const held = await ls();
  check('EVIOCGKEY has A down', held[0].includes(' down 1 '), held[0]);

  // Any other controller, under its own name and ids.
  list[1] = pad(1, 'Foo Stick (Vendor: 1234 Product: 5678)', '', 3, 2);
  await until(() => said.includes('connect 1'), 'the second connect');
  const two = await ls();
  eq('the other controller', two[1], 'event1 Foo Stick 1234:5678 keys 3 abs 2 x -32768..32767 down 0 ev 0b');
  // Firefox's form of the id.
  list[2] = pad(2, '54c-ce6-Wireless Controller', '', 2, 2);
  await until(() => said.includes('connect 2'), 'the third connect');
  const three = await ls();
  eq('ids from Firefox', three[2], 'event2 Wireless Controller 054c:0ce6 keys 2 abs 2 x -32768..32767 down 0 ev 0b');
  list[2] = null;
  await until(() => said.includes('disconnect 2'), 'the third gone');

  // The first goes: its reader is told, and its node goes.
  list[0] = null;
  await until(() => out.includes('gone'), 'the reader told', 5000).catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(out)}`); });
  eq('the reader ends', (await timeout(r.exited, 5000, 'the reader')).code, 0);
  const after = await ls();
  eq('only event1 is left', after.filter((l) => l.startsWith('event')).map((l) => l.split(' ')[0]), ['event1']);
  gp.close();
  eq('events', said, ['connect 0', 'connect 1', 'connect 2', 'disconnect 2', 'disconnect 0', 'disconnect 1']);
  await sys.shutdown();
});
