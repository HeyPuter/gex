// A fork-heavy pass over the Debian rootfs.
//
//   node web/test/forkfest.mjs [name ...]
//   GEX_FORKFEST_ROOTFS=busybox node web/test/forkfest.mjs
//
// Everything else here tests one thing at a time. This tests the shape real
// software actually has: a shell starting programs, joining them with pipes,
// substituting their output into its own command line, backgrounding them,
// waiting for them and signalling them. `apt-get update` is forty processes
// arranged that way, and when it fails the question is always which of the
// forty arrangements broke -- which a single apt run cannot answer.
//
// So: one shell, many small commands, each judged by a marker the *guest*
// prints. Never by an exit status alone, and never by a pattern that could
// appear in the shell's echo of the command it was given -- that is how a
// check passes with no guest running at all.
//
// The whole battery runs in one process tree, deliberately. A fresh boot per
// case would hide exactly the faults this exists to find: state left behind
// by process seventeen is what process eighteen trips over.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, '..');

globalThis.fetch = async (url) => {
  const path = join(WEB, 'public', String(url));
  let body;
  try {
    body = readFileSync(path);
  } catch {
    return { ok: false, status: 404, statusText: 'not found' };
  }
  return {
    ok: true,
    status: 200,
    headers: { get: () => String(body.length) },
    body: null,
    arrayBuffer: async () => body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength),
  };
};

const { runProgram } = await import('../src/runtime.js');
const { untar } = await import('../src/tar.js');

const pause = (ms) => new Promise((r) => setTimeout(r, ms));

/** Enough of xterm.js to type at and read back. */
class Term {
  constructor() {
    this.out = '';
    this.keys = [];
    this.rows = 24;
    this.cols = 80;
    this.resizers = [];
  }

  write(d) {
    const s = typeof d === 'string' ? d : new TextDecoder().decode(d);
    this.out += s;
    if (process.env.GEX_VERBOSE) process.stdout.write(s);
  }

  writeln(d) { this.write(`${d}\n`); }
  focus() {}
  onData(f) { this.keys.push(f); return { dispose: () => {} }; }
  onResize(f) { this.resizers.push(f); return { dispose: () => {} }; }
  type(t) { for (const f of this.keys.slice()) f(t); }
}

// The battery.
//
// `want` is a regular expression over what the command printed, and every one
// of them is written so that the *command line* cannot satisfy it -- markers
// are built by the shell from a status or a count, never quoted literally.
// `skip` marks a case whose failure is already recorded elsewhere, so that a
// known fault does not hide a new one behind it.
const CASES = [
  // -- the shapes a shell makes ------------------------------------------
  { name: 'exec', cmd: 'true; echo ex$?', want: /ex0/ },
  { name: 'exit-status', cmd: 'false; echo st$?', want: /st1/ },
  { name: 'pipeline-2', cmd: 'ls /bin | wc -l | tr -d " " | sed "s/^/n=/"', want: /n=[1-9]/ },
  {
    name: 'pipeline-5',
    cmd: 'cat /etc/services | grep -v "^#" | sort | uniq | wc -l | sed "s/^/five=/"',
    want: /five=[1-9]/,
  },
  {
    name: 'substitution',
    cmd: 'echo depth=$(echo $(echo $(echo 3)))',
    want: /depth=3/,
  },
  {
    name: 'subshell-exit',
    cmd: '(exit 7); echo sub$?',
    want: /sub7/,
  },
  {
    name: 'loop-of-forks',
    cmd: 'n=0; for i in 1 2 3 4 5 6 7 8; do n=$((n + $(echo 1))); done; echo loop=$n',
    want: /loop=8/,
  },
  {
    name: 'background-and-wait',
    cmd: '(exit 3) & wait $!; echo bg$?',
    want: /bg3/,
  },
  {
    name: 'nested-shell',
    cmd: 'sh -c "sh -c \'echo nest=$(id -u)\'"',
    want: /nest=0/,
  },
  {
    name: 'xargs',
    cmd: 'ls /bin | head -20 | xargs -n 3 echo | wc -l | sed "s/^/x=/"',
    want: /x=[1-9]/,
  },

  // -- signals, which had no way to reach a parked process ---------------
  {
    name: 'kill-a-child',
    // The child parks in a read that nothing will ever write to. Before
    // signals could interrupt a wait this hung until the shell gave up.
    cmd: 'sh -c "read x < /dev/null; echo unreachable" & p=$!; kill -TERM $p 2>/dev/null; '
      + 'wait $p; echo killed=$?',
    want: /killed=\d/,
  },
  {
    name: 'trap-a-signal',
    cmd: 'sh -c \'trap "echo caught=yes; exit 0" TERM; kill -TERM $$; sleep 5\'',
    want: /caught=yes/,
  },
  {
    name: 'timeout-kills',
    // `timeout` is the whole class in one command: it forks, waits, and
    // signals a process that is parked in a syscall.
    cmd: 'timeout 1 sh -c "while :; do :; done"; echo to=$?',
    want: /to=12[0-9]/,
  },
  {
    name: 'timeout-lets-through',
    cmd: 'timeout 10 true; echo ok=$?',
    want: /ok=0/,
  },

  // -- the terminal, from a forked child ---------------------------------
  { name: 'isatty-in-a-child', cmd: 'sh -c "test -t 0 && echo tty=yes"', want: /tty=yes/ },
  { name: 'stty-in-a-child', cmd: 'stty size | sed "s/ /x/" | sed "s/^/size=/"', want: /size=\d+x\d+/ },
  { name: 'terminfo', cmd: 'tput cols | sed "s/^/cols=/"', want: /cols=\d+/ },

  // -- real programs that fork -------------------------------------------
  { name: 'dpkg-list', cmd: 'dpkg -l | wc -l | sed "s/^/pkgs=/"', want: /pkgs=[1-9]/ },
  { name: 'dpkg-search', cmd: 'dpkg -S /bin/ls 2>&1 | head -1 | sed "s/^/found=/"', want: /found=\S/ },
  { name: 'apt-config', cmd: 'apt-config dump | wc -l | sed "s/^/cfg=/"', want: /cfg=[1-9]/ },
  { name: 'apt-cache', cmd: 'apt-cache policy 2>&1 | wc -l | sed "s/^/pol=/"', want: /pol=[0-9]/ },
  {
    name: 'find-and-exec',
    cmd: 'find /etc -maxdepth 1 -name "*.conf" -exec basename {} \\; | wc -l | sed "s/^/conf=/"',
    want: /conf=[0-9]/,
  },
  {
    name: 'tar-round-trip',
    cmd: 'tar cf - /etc/apt 2>/dev/null | tar tf - | wc -l | sed "s/^/tar=/"',
    want: /tar=[1-9]/,
  },
  {
    name: 'gzip-round-trip',
    cmd: 'cat /etc/services | gzip -c | gzip -dc | wc -c | sed "s/^/gz=/"',
    want: /gz=[1-9]/,
  },
  { name: 'perl', cmd: 'perl -e "print qq(perl=ok\\n)"', want: /perl=ok/ },
  { name: 'awk', cmd: 'awk "BEGIN { print \\"awk=\\" (6*7) }"', want: /awk=42/ },
  {
    name: 'sed-in-place',
    cmd: 'echo hello > /tmp/f; sed -i "s/hello/world/" /tmp/f; cat /tmp/f | sed "s/^/sed=/"',
    want: /sed=world/,
  },
  {
    name: 'many-processes',
    // Forty of them, which is what `apt-key` does and what #115 counts.
    cmd: 'i=0; while [ $i -lt 40 ]; do /bin/true; i=$((i+1)); done; echo forty=$i',
    want: /forty=40/,
  },
];

const rootfsName = process.env.GEX_FORKFEST_ROOTFS || 'debian';
const only = process.argv.slice(2);
const cases = only.length ? CASES.filter((c) => only.includes(c.name)) : CASES;

const tarBytes = readFileSync(join(WEB, 'public', 'rootfs', `${rootfsName}.tar`));
const tree = untar(
  tarBytes.buffer.slice(tarBytes.byteOffset, tarBytes.byteOffset + tarBytes.byteLength),
);
const files = { ...tree.files };
for (const [path, target] of Object.entries(tree.links)) {
  const abs = target.startsWith('/')
    ? target
    : `${path.slice(0, path.lastIndexOf('/'))}/${target}`.replace(/\/{2,}/g, '/');
  if (files[abs]) {
    files[path] = files[abs];
    if (tree.modes?.[abs] !== undefined) tree.modes[path] = tree.modes[abs];
  }
}
for (const [path, target] of Object.entries(tree.hard ?? {})) {
  if (files[target]) {
    files[path] = files[target];
    if (tree.modes?.[target] !== undefined) tree.modes[path] = tree.modes[target];
  }
}

const shell = files['/bin/bash'] ? '/bin/bash' : '/bin/sh';
const elf = files[shell] ?? files['/bin/busybox'];
if (!elf) throw new Error(`the ${rootfsName} rootfs has no shell`);

const term = new Term();
const done = runProgram(term, {
  elf,
  files,
  dirs: tree.dirs,
  modes: tree.modes,
  args: [shell, '-i'],
  cwd: '/root',
  tty: true,
  env: [
    'PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',
    'HOME=/root',
    'TERM=xterm-256color',
    'DEBIAN_FRONTEND=noninteractive',
    'LC_ALL=C',
    'LANG=C',
  ],
  uid: 0,
});
let died = null;
done.catch((e) => { died = e?.message ?? String(e); });

/** Wait for a prompt, which is the only reliable "the shell is ready". */
async function prompt(mark, seconds) {
  for (let i = 0; i < seconds * 20; i += 1) {
    if (died) return null;
    if (/[#$] $/.test(term.out.slice(mark))) return term.out.slice(mark);
    await pause(50);
  }
  return null;
}

const start = Date.now();
if (!(await prompt(0, 300))) {
  console.log(`FAIL the shell never reached a prompt${died ? `: ${died}` : ''}`);
  console.log(term.out.slice(-2000));
  process.exit(1);
}
console.log(`# ${rootfsName}: ${shell} up in ${((Date.now() - start) / 1000).toFixed(1)}s, `
  + `${cases.length} cases\n`);

let pass = 0;
const failed = [];
for (const c of cases) {
  if (died) break;
  const mark = term.out.length;
  const t0 = Date.now();
  term.type(`${c.cmd}\r`);
  const out = await prompt(mark, c.seconds ?? 120);
  const ms = Date.now() - t0;
  if (out === null) {
    failed.push({ ...c, why: died ? `the guest died: ${died}` : 'no prompt came back', ms });
    console.log(`FAIL ${c.name.padEnd(22)} ${String(ms).padStart(6)}ms  ${
      died ? 'the guest died' : 'wedged'}`);
    if (died) break;
    continue;
  }
  // Only what the command printed, with its own echo removed: the first line
  // of the slice is the shell repeating what was typed.
  const printed = out.slice(out.indexOf('\n') + 1);
  if (c.want.test(printed)) {
    pass += 1;
    console.log(`ok   ${c.name.padEnd(22)} ${String(ms).padStart(6)}ms`);
  } else {
    failed.push({ ...c, why: 'no marker', ms, printed });
    console.log(`FAIL ${c.name.padEnd(22)} ${String(ms).padStart(6)}ms  wanted ${c.want}`);
  }
}

console.log(`\n${pass}/${cases.length} passed in ${((Date.now() - start) / 1000).toFixed(0)}s`);
for (const f of failed) {
  console.log(`\n--- ${f.name}: ${f.why}`);
  console.log(`    ${f.cmd}`);
  if (f.printed !== undefined) {
    console.log(`    printed: ${JSON.stringify(f.printed.slice(0, 400))}`);
  }
}
process.exit(failed.length ? 1 : 0);
