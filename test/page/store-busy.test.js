// An OPFS store open in another page of the site (a second tab): one page
// holds a store at a time (a Web Lock named for it, hosts/browser/js/blocks.mjs
// `claimStore`). The other page here is a Worker holding the lock and the
// store's sync access handle, as a second tab's store Worker would. A page
// runs one system, so each phase is a load of its own:
//   1. a System on the busy store is told at once (EBUSY), not after the
//      handle's retries;
//   2. start() without `onStoreBusy`: the same error, before any fetch;
//      with it, the page is asked, and runs without keeping files;
//   3. the page waits instead: the other page closes, and start-up goes on
//      by itself on the store, the question taken down;
//   4. what phase 3 wrote is in the store.
import { System } from '../../page/runtime/src/index.js';
import { OPFSFilesystem } from '../../page/runtime/src/fs.js';
import { start } from '../../page/src/app.js';

export const meta = {
  hosts: ['browser'], images: ['busybox'], timeout: 240,
  html: '<div id="app" style="position:relative;width:320px;height:240px"></div>',
};

/** Another page on the store `name`: its lock and its file's handle, held
 *  until `close()`. */
async function otherPage(name) {
  const src = `onmessage = async ({ data: name }) => {
    navigator.locks.request('gex-fs-' + name, async () => {
      const root = await navigator.storage.getDirectory();
      const dir = await root.getDirectoryHandle('gex-fs-' + name, { create: true });
      const h = await (await dir.getFileHandle('blocks', { create: true })).createSyncAccessHandle();
      postMessage('held');
      await new Promise(() => {});
    });
  };`;
  const w = new Worker(URL.createObjectURL(new Blob([src], { type: 'text/javascript' })));
  await new Promise((r) => { w.onmessage = r; w.postMessage(name); });
  return { close: () => w.terminate() };
}

export default async function (t) {
  const { check, eq, timeout, until } = t;
  const { arch, ...switches } = t.debug();
  const STORE = `gex-test-busy-${t.arch}`;
  if (t.phase === 1) await OPFSFilesystem.erase(STORE);
  const zstd = new URL('../vendor/zstddec/zstddec-stream.mjs', new URL(t.baseUrl, location.href)).href;
  const build = (cmd) => ({
    arch: arch ?? 'aarch64', tar: [t.work('bb.tar')], zstd, exe: '/bin/sh', argv: ['-c', cmd],
    env: ['PATH=/bin:/usr/bin', 'HOME=/root'], cwd: '/', featuresOff: [], compositor: false,
    jitFallback: true, gl: false, switches,
  });
  const run = (cmd, more) => start({
    container: document.getElementById('app'), size: { w: 320, h: 240 }, baseUrl: t.baseUrl,
    filesystem: { kind: 'opfs', name: STORE }, build: build(cmd), ...more,
  });

  if (t.phase === 1) {
    await otherPage(STORE);
    const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
    new OPFSFilesystem(sys, { name: STORE });
    const t0 = performance.now();
    const e = await timeout(sys.boot().then(() => null, (x) => x), 60000, 'boot');
    const ms = performance.now() - t0;
    check('a System on a busy store: EBUSY', e?.code === 'EBUSY' && /open in another tab/.test(e.message), String(e?.message ?? e));
    check('said at once, not after the handle\'s 10 s of retries', ms < 8000, `${ms.toFixed(0)} ms`);
    return t.reload();
  }
  if (t.phase === 2) {
    await otherPage(STORE);
    const e = await timeout(run('true').then(() => null, (x) => x), 30000, 'start');
    check('start() on a busy store, no onStoreBusy: EBUSY', e?.code === 'EBUSY', String(e?.message ?? e));
    const asked = [];
    const app = await timeout(run('echo kept > /root/f; cat /root/f', {
      onStoreBusy: ({ name, signal }) => { asked.push({ name, signal }); return 'memory'; },
    }), 60000, 'start (memory)');
    eq('the page was asked, with the store\'s name', asked.map((a) => a.name), [STORE]);
    eq('it ran without keeping files', await timeout(app.exited, 60000, 'exit'), 0);
    check('its output', /^kept$/m.test(app.state.out.replace(/\r/g, '')), app.state.out.slice(-200));
    return t.reload();
  }
  if (t.phase === 3) {
    const other = await otherPage(STORE);
    let signal = null;
    const going = run('echo kept > /root/f; sync; cat /root/f', {
      onStoreBusy: (b) => { signal = b.signal; return 'wait'; },
    });
    await until(() => signal, 'the question', 10000);
    check('waiting: the question still up', !signal.aborted);
    other.close();
    const app = await timeout(going, 90000, 'start (waited)');
    check('the other page closed: the question taken down', signal.aborted);
    eq('it ran on the store', await timeout(app.exited, 60000, 'exit'), 0);
    await new Promise((r) => setTimeout(r, 6000));
    return t.reload();
  }
  const app = await timeout(run('cat /root/f'), 60000, 'start (again)');
  eq('a load after: the store free', await timeout(app.exited, 60000, 'exit'), 0);
  check('what phase 3 kept', /^kept$/m.test(app.state.out.replace(/\r/g, '')), app.state.out.slice(-200));
  await OPFSFilesystem.erase(STORE).catch(() => {});
}
