// A failed fetch of the other memory flavour's runtime is not remembered for
// good: the next exec that needs it, once a backoff has passed, fetches it
// again and runs. The guest (test/fixtures/programs/mem64-retry.c) spawns a
// program in a 64-bit memory while the fetch fails, again at once (failed
// inside the backoff, with no second fetch), and, after the test makes the
// runtime available, again after the backoff. In the browser the package
// (page/runtime/build.sh), its first fetch of the memory64 shim failing as a
// dropped connection would (the test server's `/__t/drop`, which reaches the
// single-thread build's system Worker too); on the Node host a site laid out
// as gex's whose memory64 shim is linked in only after the first failure.
export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['busybox', 'mem64', 'package'], timeout: 180 };

const WANT = `first: No such file or directory
again at once: No such file or directory
waiting
ran
after the backoff: exit 0`;
const M64 = 'gex_browser.x86.m64.shared';

export default async function (t) {
  const { eq, timeout } = t;
  const browser = typeof document !== 'undefined';
  let MemoryFilesystem;
  let sys;
  let allow;
  // The package's memory64 shim as the test server was asked for it, from
  // here on: the first ask fails.
  const asked = async () => Number(await (await fetch(`/__t/asked?${encodeURIComponent(`pkg/${M64}`)}`)).text());
  let before = 0;
  const fetches = async () => (await asked()) - before;
  if (browser) {
    before = await asked();
    await fetch(`/__t/drop?${encodeURIComponent(`pkg/${M64}`)}`);
    allow = () => {};
    const pkg = t.work('pkg/');
    const { System } = await import(/* @vite-ignore */ `${pkg}index.mjs`);
    ({ MemoryFilesystem } = await import(/* @vite-ignore */ `${pkg}fs.mjs`));
    sys = new System({ debug: t.debug() });
  } else {
    const { join } = await import('node:path');
    const { mkdirSync, readdirSync, rmSync, symlinkSync } = await import('node:fs');
    const { pathToFileURL } = await import('node:url');
    const pub = join(import.meta.dirname, '../../page/public/gex'), site = join(t.workDir, 'mem64-retry-site');
    rmSync(site, { recursive: true, force: true });
    mkdirSync(join(site, 'gex'), { recursive: true });
    const m64 = readdirSync(pub).filter((f) => f.startsWith(M64));
    for (const f of readdirSync(pub)) if (!m64.includes(f)) symlinkSync(join(pub, f), join(site, 'gex', f));
    allow = () => { for (const f of m64) symlinkSync(join(pub, f), join(site, 'gex', f)); };
    const { System } = await import('../../page/runtime/src/index.js');
    ({ MemoryFilesystem } = await import('../../page/runtime/src/fs.js'));
    sys = new System({ baseUrl: `${pathToFileURL(site).href}/`, debug: t.debug() });
  }
  try {
    const fs = new MemoryFilesystem(sys);
    fs.addTar(t.work('bb.tar.zst'));
    fs.addFiles({ '/g/retry': new URL(t.work('mem64-retry.x86')) }, { mode: 0o755 });
    await timeout(sys.boot(), 60000, 'boot');
    const p = sys.spawn(['/g/retry'], { env: { HOME: '/' } });
    const reader = p.stdout.pipeThrough(new TextDecoderStream()).getReader();
    let out = '';
    // Up to `s`, or with none, to the end.
    const until = async (s) => {
      while (!s || !out.includes(s)) {
        const { done, value } = await reader.read();
        if (done) break;
        out += value;
      }
    };
    await timeout(until('waiting\n'), 60000, 'the failed spawns');
    if (browser) eq('one fetch while the backoff lasts', await fetches(), 1);
    allow();
    const w = p.stdin.getWriter();
    await w.write(new TextEncoder().encode('go\n'));
    await timeout(until(), 60000, 'the spawn after the backoff');
    const r = await timeout(p.exited, 30000, 'exit');
    eq('fails while the fetch fails, runs once it is had', out.trim(), WANT);
    eq('and exits 0', r.code, 0);
    if (browser) eq('fetched again after the backoff', await fetches(), 2);
  } finally {
    await sys.shutdown();
  }
}
