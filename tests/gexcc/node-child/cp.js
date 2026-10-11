// child_process over the guest's socketpairs (libuv: uv_spawn's stdio is
// socketpair(AF_UNIX, SOCK_STREAM); uv_pipe_open takes readability and
// writability from F_GETFL; uv_guess_handle uses SO_TYPE/getsockname;
// spawnSync's input ends with shutdown(SHUT_WR)). One line per check.
const cp = require('child_process');
const fs = require('fs');
const ok = (name, cond, got) => console.log(`${cond ? 'ok' : 'FAIL'} ${name}${cond ? '' : ': ' + JSON.stringify(got)}`);
let r;
// The child's stdio as the child sees it: a socket, and a pipe to libuv.
r = cp.spawnSync('node', ['-e', "console.log(fs.fstatSync(1).isSocket(), process.stdout._type, process.stdin._type ?? 'lazy')"]);
ok('child stdout is a socket and a pipe to libuv', r.status === 0 && /^true pipe/.test(String(r.stdout)), [r.status, String(r.stdout), String(r.stderr)]);
// Input written and ended (SHUT_WR): the child reads it to EOF.
r = cp.spawnSync('node', ['-e', "let b='';process.stdin.on('data',d=>b+=d).on('end',()=>console.log('got',b.trim()))"], { input: 'via stdin\n' });
ok('spawnSync input reaches EOF', r.status === 0 && String(r.stdout).trim() === 'got via stdin', [r.status, String(r.stdout), String(r.stderr)]);
// stderr is its own pair.
r = cp.spawnSync('node', ['-e', "console.error('to stderr'); process.exit(3)"]);
ok('stderr captured, status kept', r.status === 3 && String(r.stderr).trim() === 'to stderr' && String(r.stdout) === '', [r.status, String(r.stdout), String(r.stderr)]);
// Through a shell: sh -c then exec of node.
r = cp.execSync('node -p "6*7"').toString().trim();
ok('execSync via sh', r === '42', r);
// A large write through the pair (several pipe buffers).
r = cp.spawnSync('node', ['-e', "process.stdout.write('y'.repeat(1<<20))"], { maxBuffer: 4 << 20 });
ok('1 MiB from the child', r.status === 0 && r.stdout.length === 1 << 20, [r.status, r.stdout && r.stdout.length]);
// Another program (nvim when the root has it).
if (fs.existsSync('/usr/bin/nvim')) {
  r = cp.spawnSync('/usr/bin/nvim', ['--version']);
  ok('spawnSync nvim --version', r.status === 0 && /^NVIM v/.test(String(r.stdout)), [r.status, String(r.stdout).slice(0, 80), String(r.stderr).slice(0, 200)]);
}
// Async: data events, then close with the status; stdin written and ended.
const c = cp.spawn('node', ['-e', "process.stdin.pipe(process.stdout)"]);
let o = '';
c.stdout.on('data', (d) => (o += d));
c.on('close', (s) => {
  ok('async spawn: echo through stdin and stdout', s === 0 && o === 'ping pong\n', [s, o]);
  const d = cp.spawn('node', ['-p', '6*7']);
  let p = '';
  d.stdout.on('data', (x) => (p += x));
  d.on('close', (s2) => { ok('async spawn: node -p', s2 === 0 && p.trim() === '42', [s2, p]); console.log('END'); });
});
c.stdin.end('ping pong\n');
