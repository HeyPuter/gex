// The page at either end of a program's unix stream socket: sys.listen and
// sys.connect, with fixtures/probe/rt-unix.c the program.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 300 };

const enc = new TextEncoder();
const dec = new TextDecoder();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function text(readable) {
  let s = '';
  for (const r = readable.getReader(); ;) {
    const { value, done } = await r.read();
    if (done) return s;
    s += dec.decode(value, { stream: true });
  }
}

async function send(conn, s) {
  const w = conn.writable.getWriter();
  await w.write(s);
  await w.close();
}

export default async function (t) {
  const { say, check, eq, timeout, until, lines } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };
  const run = (argv, o = {}) => timeout(sys.run(argv, { env, ...o }), 60000, argv.join(' '));
  const errOf = (p) => p.then(() => 'resolved', (e) => e.code ?? e.message);

  // Made before boot: bound as the kernel comes up.
  let threw = null;
  try { sys.listen('relative.sock'); } catch (e) { threw = e; }
  check('a relative path is a TypeError', threw instanceof TypeError, String(threw));
  const early = sys.listen('/tmp/early.sock');
  await timeout(sys.boot(), 60000, 'boot');
  await timeout(early.ready, 10000, 'early.ready');
  const ls = await run(['ls', '-l', '/tmp/early.sock']);
  check('listening before boot: the socket file is there', ls.code === 0 && ls.stdout.startsWith('s'), JSON.stringify(ls));

  // A program's connect to it: what it sends to its shutdown(SHUT_WR), then
  // the page's answer back after the readable ended.
  {
    const client = run(['rt-unix', 'connect', '/tmp/early.sock', 'hello page']);
    const conn = await timeout(early.accept(), 20000, 'accept');
    eq('the program\'s bytes, to its SHUT_WR', await timeout(text(conn.readable), 20000, 'read'), 'hello page');
    await send(conn, 'written after the program\'s half close');
    const r = await client;
    eq('the program reads the page\'s answer after its own SHUT_WR', [r.code, r.stdout], [0, 'written after the program\'s half close']);
  }

  // After boot, with for await.
  const srv = sys.listen('/tmp/app.sock');
  await srv.ready;
  const conns = [];
  const taking = (async () => {
    for await (const c of srv) conns.push(c);
  })();

  // The page's half close: the program reads 0, and can still write.
  {
    const p = sys.spawn(['rt-unix', 'half', '/tmp/app.sock'], { env });
    const out = lines(p);
    await until(() => conns.length === 1, 'the half connection');
    const c = conns.shift();
    await send(c, 'ping');
    eq('after the page\'s writable.close(), the program\'s read is 0 (select)', await timeout(text(c.readable), 20000, 'after eof'), 'after eof\n');
    await timeout(p.exited, 20000, 'half exit');
    await until(() => out.length, 'its line');
    eq('it said what it got', out, ['got ping']);
  }

  // Three at once: each accepted before any is answered.
  {
    const runs = [1, 2, 3].map((i) => run(['rt-unix', 'connect', '/tmp/app.sock', `c${i}`]));
    await until(() => conns.length === 3, 'three connections', 30000);
    const got = await Promise.all(conns.splice(0).map(async (c) => {
      const s = await timeout(text(c.readable), 20000, 'one of three');
      await send(c, s.toUpperCase());
      return s;
    }));
    eq('three connections at once, each its own', got.sort(), ['c1', 'c2', 'c3']);
    const rs = await Promise.all(runs);
    eq('and each program its own answer', rs.map((r) => r.stdout).sort(), ['C1', 'C2', 'C3']);
  }

  // A SOCK_SEQPACKET connect is a stream here; a datagram to the name fails.
  {
    const r = run(['rt-unix', 'seqpacket', '/tmp/app.sock', 'sp']);
    await until(() => conns.length === 1, 'the seqpacket connection');
    const c = conns.shift();
    const s = await timeout(text(c.readable), 20000, 'seqpacket');
    await send(c, s.toUpperCase());
    eq('a SOCK_SEQPACKET connect', [(await r).code, (await r).stdout], [0, 'SP']);
    const d = await run(['rt-unix', 'dgram', '/tmp/app.sock', 'x']);
    check('a SOCK_DGRAM send to the page\'s name fails', d.code === 1 && /^sendto: /.test(d.stdout), JSON.stringify(d));
    say(`  datagram: ${d.stdout.trim()}`);
  }

  // Backpressure, the program writing: a page that stops reading stops it.
  {
    const MIB = 64;
    const p = sys.spawn(['rt-unix', 'write', '/tmp/app.sock', String(MIB)], { env });
    const out = lines(p);
    await until(() => conns.length === 1, 'the writer');
    const c = conns.shift();
    const r = c.readable.getReader();
    let n = 0;
    let bad = -1;
    const take = (b) => {
      for (let i = 0; i < b.length && bad < 0; i += 4093) if (b[i] !== (n + i) % 251) bad = n + i;
      n += b.length;
    };
    take((await r.read()).value);
    await sleep(1500);
    const held = out.length;
    await sleep(500);
    eq('a reader that pauses stops the writer (its count does not move)', out.length, held);
    check('the writer is held within a MiB or two', held <= 2, `${held} MiB written while the page read ${n} bytes`);
    const t0 = performance.now();
    for (;;) {
      const { value, done } = await timeout(r.read(), 30000, 'flood read');
      if (done) break;
      take(value);
    }
    const ms = performance.now() - t0;
    eq(`${MIB} MiB through, every byte where it belongs`, [n, bad], [MIB << 20, -1]);
    await timeout(p.exited, 30000, 'writer exit');
    await until(() => out.includes(`wrote ${MIB}`), 'the writer wrote it all');
    say(`  ${MIB} MiB program to page in ${ms.toFixed(0)} ms (${(MIB / (ms / 1000)).toFixed(1)} MiB/s)`);
  }

  // Backpressure, the page writing: its writes wait on a program that does
  // not read yet.
  {
    const p = run(['rt-unix', 'slowread', '/tmp/app.sock', '1500']);
    await until(() => conns.length === 1, 'the slow reader');
    const c = conns.shift();
    const w = c.writable.getWriter();
    const chunk = new Uint8Array(65536);
    let taken = 0;
    const writes = (async () => {
      for (let i = 0; i < 64; i++) {
        await w.write(chunk);
        taken += chunk.length;
      }
      await w.close();
    })();
    await sleep(800);
    const early = taken;
    check('the page\'s writes wait on a program not reading', early <= (1 << 20), `${early} bytes taken before it read`);
    await timeout(writes, 30000, 'page writes');
    const r = await p;
    eq('then it reads them all', [r.code, r.stdout], [0, `read ${64 * 65536}\n`]);
    c.close();
  }

  // An abort: the program reads the end, then its writes fail with EPIPE.
  {
    const p = run(['rt-unix', 'pipe', '/tmp/app.sock']);
    await until(() => conns.length === 1, 'the pipe connection');
    const c = conns.shift();
    await c.writable.abort();
    const r = await p;
    eq('writable.abort(): the program reads 0, then EPIPE', [r.code, r.stdout], [0, 'eof\nEPIPE\n']);
  }

  // A program that dies: the page's readable ends; its writes then fail.
  {
    const p = sys.spawn(['rt-unix', 'write', '/tmp/app.sock', '4096'], { env });
    await until(() => conns.length === 1, 'the doomed writer');
    const c = conns.shift();
    const r = c.readable.getReader();
    await r.read();
    p.kill('SIGKILL');
    await timeout(p.exited, 20000, 'killed');
    let end;
    for (;;) {
      end = await timeout(r.read().then((x) => x, (e) => ({ error: e })), 20000, 'after the kill');
      if (end.done || end.error) break;
    }
    check('a killed program: the readable ends', end.done === true, String(end.error));
    const w = c.writable.getWriter();
    eq('and a write to it fails with EPIPE', await errOf(w.write('x')), 'EPIPE');
  }

  // Abstract names too.
  {
    const ab = sys.listen('\0gex-unix-test');
    await ab.ready;
    const r = run(['rt-unix', 'connect', '@gex-unix-test', 'abstract']);
    const c = await timeout(ab.accept(), 20000, 'abstract accept');
    await send(c, (await text(c.readable)).toUpperCase());
    eq('an abstract name', (await r).stdout, 'ABSTRACT');
    await ab.close();
    eq('closed, an abstract name is refused', (await run(['rt-unix', 'connect', '@gex-unix-test', 'x'])).stdout, 'connect: ECONNREFUSED\n');
  }

  // Names already taken, and missing directories.
  await fs.writeFile('/tmp/plain', 'x');
  eq('a file there: EADDRINUSE', await errOf(sys.listen('/tmp/plain').ready), 'EADDRINUSE');
  eq('a name listened on: EADDRINUSE', await errOf(sys.listen('/tmp/app.sock').ready), 'EADDRINUSE');
  eq('no such directory: ENOENT', await errOf(sys.listen('/nowhere/x.sock').ready), 'ENOENT');
  const failed = sys.listen('/nowhere/y.sock');
  eq('accept on one that could not bind rejects', await errOf(failed.accept()), 'ENOENT');

  // Closed: refused, the file stays; listening there again takes it.
  await srv.close();
  await timeout(taking, 10000, 'the for await ends');
  const refused = await run(['rt-unix', 'connect', '/tmp/app.sock', 'x']);
  eq('after close, a program\'s connect is refused', [refused.code, refused.stdout], [1, 'connect: ECONNREFUSED\n']);
  const kept = await run(['ls', '-l', '/tmp/app.sock']);
  check('and the socket file stays', kept.code === 0 && kept.stdout.startsWith('s'), JSON.stringify(kept));
  const again = sys.listen('/tmp/app.sock');
  await timeout(again.ready, 10000, 'listen again');
  {
    const r = run(['rt-unix', 'connect', '/tmp/app.sock', 'again']);
    const c = await timeout(again.accept(), 20000, 'accept again');
    await send(c, await text(c.readable));
    eq('a stale socket file is listened on again', (await r).stdout, 'again');
  }
  // Leaving a for await closes it.
  {
    const r = run(['rt-unix', 'connect', '/tmp/app.sock', 'last']);
    for await (const c of again) {
      await send(c, await text(c.readable));
      break;
    }
    eq('the last one through', (await r).stdout, 'last');
    eq('and the loop\'s break closed the listener', [again.closed, (await run(['rt-unix', 'connect', '/tmp/app.sock', 'x'])).stdout], [true, 'connect: ECONNREFUSED\n']);
  }

  // The page connecting to a program's server (epoll, non-blocking).
  {
    const p = sys.spawn(['rt-unix', 'serve', '/tmp/srv.sock', '3'], { env });
    const out = lines(p);
    await until(() => out.includes('listening'), 'rt-unix serve');
    const cs = await Promise.all([1, 2, 3].map(() => timeout(sys.connect('/tmp/srv.sock'), 20000, 'connect')));
    // Each written, then half closed, the replies read only after all three
    // are open: the server holds three at once.
    const replies = await Promise.all(cs.map(async (c, i) => {
      await send(c, `to the guest ${i}`);
      return timeout(text(c.readable), 20000, `reply ${i}`);
    }));
    eq('sys.connect: three at once, each answered', replies, ['TO THE GUEST 0', 'TO THE GUEST 1', 'TO THE GUEST 2']);
    eq('the server served them', (await timeout(p.exited, 20000, 'serve exit')).code, 0);
    await until(() => out.includes('served 3'), 'the server says it served 3');
    eq('its socket file left, nobody listening: ECONNREFUSED', await errOf(sys.connect('/tmp/srv.sock')), 'ECONNREFUSED');
    eq('no file at all: ENOENT', await errOf(sys.connect('/tmp/none.sock')), 'ENOENT');
  }

  // A program's server that the page writes a lot to (backpressure through
  // its reads) and that answers.
  {
    const p = sys.spawn(['rt-unix', 'serve', '@gex-big', '1'], { env });
    const out = lines(p);
    await until(() => out.includes('listening'), 'rt-unix serve big');
    const c = await sys.connect('\0gex-big');
    await send(c, 'a'.repeat(4000));
    eq('an abstract name a program listens on', await timeout(text(c.readable), 20000, 'big reply'), 'A'.repeat(4000));
    await timeout(p.exited, 20000, 'serve big exit');
  }

  await sys.shutdown();
  eq('after shutdown, listen throws', (() => { try { sys.listen('/tmp/z.sock'); return 'no'; } catch { return 'threw'; } })(), 'threw');
}
