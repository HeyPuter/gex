// The demos. Each has an `enter(ctx)` that owns the terminal until the next is
// chosen; `ctx` carries the page's chrome so a demo never touches the DOM.

const running = (on) => window.dispatchEvent(new CustomEvent('gex:running', { detail: on }));

/** Print a rule with a label, so a long session stays readable. */
function rule(ctx, label) {
  const w = Math.max(20, ctx.term.cols - label.length - 4);
  ctx.term.writeln(ctx.ansi.dim(`── ${label} ${'─'.repeat(w)}`));
}

// Resolve a rootfs symlink target without letting `..` escape the archive
// root. Debian commonly uses links such as ../../share/foo; merely collapsing
// duplicate slashes leaves those targets impossible to find in `tree.files`.
function rootfsLinkTarget(path, target) {
  const raw = target.startsWith('/')
    ? target
    : `${path.slice(0, path.lastIndexOf('/'))}/${target}`;
  const parts = [];
  for (const part of raw.split('/')) {
    if (!part || part === '.') continue;
    if (part === '..') parts.pop();
    else parts.push(part);
  }
  return `/${parts.join('/')}`;
}


// A demo that failed. The console gets the error object itself so devtools can
// expand a real stack; the terminal gets the message and the top of the stack.
function failed(ctx, e) {
  const msg = e?.message ?? String(e);
  console.error('[gex] the demo failed:', e);
  try {
    ctx.term.writeln('');
    ctx.term.writeln(ctx.ansi.red(`gex: ${msg}`));
    for (const line of String(e?.stack ?? '').split('\n').slice(1, 9)) {
      if (line.trim()) ctx.term.writeln(ctx.ansi.dim(`    ${line.trim()}`));
    }
    if (e?.cause) ctx.term.writeln(ctx.ansi.dim(`    caused by ${e.cause}`));
  } catch {
    // The terminal is the nicety; the console above is the guarantee.
  }
  ctx.status(msg, 'bad');
}

// -- 1: any binary --------------------------------------------------------

const upload = {
  id: 'upload',
  title: 'Run your own binary',
  blurb:
    'Drop in a statically linked aarch64 Linux executable. Nothing is sent anywhere: the page ' +
    'translates it to WebAssembly as it runs.',
  network: false,

  async enter(ctx) {
    const input = document.createElement('input');
    input.type = 'file';
    input.style.display = 'none';
    document.body.append(input);

    const pick = () => input.click();
    ctx.actions([{ label: 'Choose a binary…', primary: true, onClick: pick }]);
    ctx.hint('or drop a file onto the terminal');

    ctx.term.writeln('Give it a static aarch64 ELF and it will run here.');
    ctx.term.writeln('');
    ctx.term.writeln(ctx.ansi.dim('  aarch64-linux-musl-gcc -static -O2 -o hello hello.c'));
    ctx.term.writeln('');
    ctx.term.writeln(
      ctx.ansi.dim('There is no ahead-of-time step: the page ships one 1.4 KB module that'),
    );
    ctx.term.writeln(
      ctx.ansi.dim('translates nothing, and every basic block is lifted and compiled the'),
    );
    ctx.term.writeln(ctx.ansi.dim('first time control reaches it.'));
    ctx.term.writeln('');

    const run = async (file) => {
      const buf = new Uint8Array(await file.arrayBuffer());
      if (buf.length < 20 || buf[0] !== 0x7f || buf[1] !== 0x45) {
        ctx.term.writeln(ctx.ansi.red(`${file.name} is not an ELF`));
        return;
      }
      if (buf[18] !== 0xb7) {
        ctx.term.writeln(
          ctx.ansi.red(`${file.name} is not aarch64 (e_machine=${buf[18] | (buf[19] << 8)})`),
        );
        return;
      }
      const argline = prompt(`Arguments for ${file.name}`, '') ?? '';
      const args = [file.name, ...argline.split(/\s+/).filter(Boolean)];
      rule(ctx, `${file.name} — ${ctx.bytes(buf.length)}`);
      ctx.status('running');
      running(true);
      const t0 = performance.now();
      try {
        const { status } = await ctx.runProgram(ctx.term, {
          elf: buf,
          files: {},
          args,
          cwd: '/',
          tty: true,
        });
        const ms = (performance.now() - t0).toFixed(0);
        ctx.term.writeln('');
        rule(ctx, `exit ${status} in ${ms} ms`);
        ctx.status(status === 0 ? 'finished' : `exit ${status}`, status === 0 ? 'good' : 'bad');
      } catch (e) {
        failed(ctx, e);
      } finally {
        running(false);
      }
    };

    input.onchange = () => input.files[0] && run(input.files[0]);

    const stage = document.querySelector('.stage');
    const stop = (e) => {
      e.preventDefault();
      e.stopPropagation();
    };
    stage.ondragover = stop;
    stage.ondrop = (e) => {
      stop(e);
      const f = e.dataTransfer?.files?.[0];
      if (f) run(f);
    };
  },
};

// -- 2: busybox -----------------------------------------------------------

const busybox = {
  id: 'busybox',
  title: 'A BusyBox userspace',
  blurb:
    'A root filesystem with one binary in it and a shell on top. Real fork, real exec, real ' +
    'pipes — the shell starts each command as its own process.',
  network: false,

  async enter(ctx) {
    ctx.actions([{ label: 'Run', primary: true, id: 'boot', onClick: () => boot(ctx) }]);
    ctx.hint('a real shell: try `ls -l /bin`, `stty -a`, `echo hi | wc -c`, ^C');
    ctx.term.writeln('A BusyBox rootfs, about 1 MB, unpacked into an in-memory filesystem.');
    ctx.term.writeln('');
    ctx.term.writeln(ctx.ansi.dim('Press Run. The first command is slow -- the whole binary has'));
    ctx.term.writeln(ctx.ansi.dim('to be translated -- and everything after it is not.'));
    ctx.term.writeln('');
  },
};

async function boot(ctx) {
  document.getElementById('boot').disabled = true;
  ctx.busy('fetching root filesystem');
  try {
    const tree = await ctx.rootfs('/rootfs/busybox.tar');
    const files = { ...tree.files };
    // A symlink becomes a copy of the target's bytes, and the mode must travel
    // with them: without it `/bin/sh` (a symlink to dash) has no execute bit
    // and dpkg reports "not found in PATH or not executable".
    for (const [path, target] of Object.entries(tree.links)) {
      const abs = rootfsLinkTarget(path, target);
      if (files[abs]) {
        files[path] = files[abs];
        if (tree.modes?.[abs] !== undefined) tree.modes[path] = tree.modes[abs];
      }
    }
    // Hard links resolve against the root of the archive, not the link's
    // directory. `/usr/bin/perl` is one.
    for (const [path, target] of Object.entries(tree.hard ?? {})) {
      if (files[target]) {
        files[path] = files[target];
        if (tree.modes?.[target] !== undefined) tree.modes[path] = tree.modes[target];
      }
    }
    const elf = files['/bin/busybox'];
    if (!elf) throw new Error('the rootfs has no /bin/busybox');
    // `?cmd=` runs one line and exits instead of opening a shell -- what the
    // test harness drives every measured case through.
    const bbCmd = new URLSearchParams(location.search).get('cmd');
    if (bbCmd) files['/cmd.sh'] = new TextEncoder().encode(`${bbCmd}\n`);
    // busybox is `static-pie` linked, so this is the only demo that runs with no
    // dynamic linker and no shared libc -- the control the Debian demo cannot
    // be, differing from it only by `ld.so`.
    ctx.busy('loading the closure');
    const { loadClosure } = await import('./closure.js');
    const wantClosure = new URLSearchParams(location.search).get('closure') !== '0';
    let closure = null;
    try {
      closure = !wantClosure ? null : await loadClosure('/closure/busybox', {
        image: '/bin/busybox',
        log: (m) => console.debug(`[gex] ${m}`),
      });
    } catch (e) {
      // No closure is the ordinary case; the demo runs on the translator alone.
      console.debug(`[gex] no busybox closure: ${e.message}`);
    }
    ctx.idle();
    ctx.term.reset();
    ctx.term.writeln(ctx.ansi.dim(`rootfs: ${Object.keys(files).length} files`));
    ctx.term.writeln('');
    ctx.status('running');
    running(true);
    const { status } = await ctx.runProgram(ctx.term, {
      elf,
      files,
      dirs: tree.dirs,
      modes: tree.modes,
      args: bbCmd ? ['/bin/sh', '/cmd.sh'] : ['/bin/sh', '-i'],
      cwd: '/root',
      // The terminal is an xterm; a shell with no `TERM` assumes a glass
      // teletype.
      env: ['TERM=xterm-256color', 'PATH=/bin:/usr/bin:/sbin:/usr/sbin', 'HOME=/root'],
      tty: true,
      closure,
    });
    ctx.term.writeln(ctx.ansi.dim(`\r\n[shell exited ${status}]`));
    ctx.status('finished', 'good');
  } catch (e) {
    failed(ctx, e);
  } finally {
    running(false);
    ctx.idle();
    const b = document.getElementById('boot');
    if (b) b.disabled = false;
  }
}

// -- 3: debian ------------------------------------------------------------

const debian = {
  id: 'debian',
  title: 'Debian, with apt',
  blurb:
    'A minimal Debian arm64 root filesystem. Networking goes out over Wisp, so apt can reach a ' +
    'real mirror.',
  network: true,

  async enter(ctx) {
    ctx.actions([
      { label: 'Run', primary: true, id: 'dboot', onClick: () => bootDebian(ctx) },
      { label: 'Wisp endpoint…', onClick: () => setWisp(ctx) },
    ]);
    ctx.hint(`network: ${wispUrl() || 'not set'}`);
    ctx.term.writeln('A Debian bookworm arm64 rootfs, built by web/build-debian.sh.');
    ctx.term.writeln('');
    ctx.term.writeln(ctx.ansi.dim('glibc is a harder target than musl -- a real dynamic loader and'));
    ctx.term.writeln(ctx.ansi.dim('far more syscalls. `apt-get update` fetches and parses the real'));
    ctx.term.writeln(ctx.ansi.dim('bookworm index; `bash`, `ls` and `dpkg` all run. Installing a'));
    ctx.term.writeln(ctx.ansi.dim('package is the next step and is not finished.'));
    ctx.term.writeln('');
    ctx.term.writeln(ctx.ansi.dim('A page cannot open a TCP socket, so network syscalls are carried'));
    ctx.term.writeln(ctx.ansi.dim('over Wisp to a proxy. Set one to give the guest a network.'));
    ctx.term.writeln('');
  },
};

const WISP_KEY = 'gex.wisp';
// `?wisp=ws://…` takes precedence over what was typed in, so a link or harness
// can carry a proxy.
const wispUrl = () =>
  new URLSearchParams(location.search).get('wisp') || localStorage.getItem(WISP_KEY) || '';

function setWisp(ctx) {
  const url = prompt(
    'Wisp endpoint (a wss:// or ws:// URL ending in a slash).\n' +
      'Leave empty for no network.',
    wispUrl(),
  );
  if (url === null) return;
  if (url) localStorage.setItem(WISP_KEY, url);
  else localStorage.removeItem(WISP_KEY);
  ctx.hint(`network: ${wispUrl() || 'not set'}`);
}

async function bootDebian(ctx) {
  document.getElementById('dboot').disabled = true;
  const cmd = new URLSearchParams(location.search).get('cmd');
  try {
    const tree = await ctx.rootfs('/rootfs/debian.tar');
    const files = { ...tree.files };
    for (const [path, target] of Object.entries(tree.links)) {
      const abs = rootfsLinkTarget(path, target);
      // A symlink becomes a copy; the mode must travel with the bytes or
      // `/bin/sh` (a symlink to dash) loses its execute bit.
      if (files[abs]) {
        files[path] = files[abs];
        if (tree.modes?.[abs] !== undefined) tree.modes[path] = tree.modes[abs];
      }
    }
    // Hard links resolve against the root of the archive, not the link's
    // directory. `/usr/bin/perl` is one.
    for (const [path, target] of Object.entries(tree.hard ?? {})) {
      if (files[target]) {
        files[path] = files[target];
        if (tree.modes?.[target] !== undefined) tree.modes[path] = tree.modes[target];
      }
    }
    if (cmd) {
      files['/cmd.sh'] = new TextEncoder().encode(`${cmd}\n`);
    }
    const elf = files['/bin/bash'] || files['/usr/bin/bash'];
    if (!elf) throw new Error('the rootfs has no /bin/bash');

    let network = null;
    if (wispUrl()) {
      ctx.busy('connecting to the proxy');
      const { WispNetwork } = await import('./net.js');
      network = new WispNetwork(wispUrl(), (e) => ctx.setNet(e.split(' ')[0]));
      await network.start();
      ctx.setNet('open');
    } else {
      ctx.setNet('off');
    }

    // A statically recompiled closure, when one has been built. `loadClosure`
    // answers `null` when there is no manifest, so trying costs nothing. The
    // image path is passed so it can be checked against the manifest: pid 1
    // here is `bash`, and handing it a module built for another program starts
    // the guest inside someone else's code and it exits without a syscall.
    // `?closure=0` runs the same demo on the runtime translator alone.
    ctx.busy('loading the closure');
    const { loadClosure } = await import('./closure.js');
    const wantClosure = new URLSearchParams(location.search).get('closure') !== '0';
    const closure = !wantClosure ? null : await loadClosure('/closure/debian', {
      image: '/bin/bash',
      // The console, not the terminal, which belongs to the guest.
      log: (s) => console.debug(`[gex] ${s}`),
    });

    ctx.idle();
    ctx.term.reset();
    ctx.term.writeln(ctx.ansi.dim(`rootfs: ${Object.keys(files).length} files`));
    if (closure) {
      ctx.term.writeln(
        ctx.ansi.dim(`closure: ${closure.libs.length} libraries, translated ahead of time`),
      );
    }
    ctx.term.writeln(
      ctx.ansi.dim(network ? 'network: wisp' : 'network: none -- apt will not reach a mirror'),
    );
    if (cmd) ctx.term.writeln(ctx.ansi.dim(`command: ${cmd}`));
    ctx.term.writeln('');
    ctx.status('running');
    running(true);
    const { status } = await ctx.runProgram(ctx.term, {
      elf,
      files,
      // The empty directories too: apt wants `.../archives/partial` and
      // `/etc/apt/preferences.d` to exist and holds no files in either.
      dirs: tree.dirs,
      modes: tree.modes,
      // `PATH` must carry the `sbin` dirs (dpkg looks for `ldconfig` and
      // `start-stop-daemon`); `DEBIAN_FRONTEND` keeps maintainer scripts from
      // waiting on a debconf dialogue nobody can answer.
      env: [
        'PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',
        'HOME=/root',
        'TERM=xterm-256color',
        'DEBIAN_FRONTEND=noninteractive',
        'LC_ALL=C',
        'LANG=C',
      ],
      // `?cmd=apt-get update` runs one command and exits instead of opening a
      // shell, so a test can drive it. Through a file rather than `-c`: `bash
      // -lc '…'` came back exit 0 having printed and run nothing.
      args: cmd ? ['/bin/bash', '/cmd.sh'] : ['/bin/bash', '-i'],
      cwd: '/root',
      tty: true,
      closure,
      network,
      onNetEvent: (e) => ctx.setNet(e.split(' ')[0]),
      // Root: who /root belongs to and who apt expects to be. Without it the
      // guest is a uid with no passwd entry and bash greets "I have no name!".
      uid: 0,
    });
    ctx.term.writeln(ctx.ansi.dim(`\r\n[bash exited ${status}]`));
    ctx.status('finished', 'good');
  } catch (e) {
    failed(ctx, e);
  } finally {
    running(false);
    ctx.idle();
    const b = document.getElementById('dboot');
    if (b) b.disabled = false;
  }
}

// -- 4: the gears ---------------------------------------------------------

const gears = {
  id: 'gears',
  title: 'OpenGL, on your GPU',
  blurb:
    'An ordinary OpenGL ES 2 program. It includes the standard headers and links against ' +
    'libEGL and libGLESv2 by their usual names -- and those libraries, inside the guest, are ' +
    'marshallers: every call crosses to this page and runs on WebGL.',
  network: false,

  async enter(ctx) {
    const { GlDevice } = await import('./gl.js');

    // The canvas lives above the terminal while this demo is chosen, made here
    // rather than in the markup so nothing else has to know the demo exists.
    const wrap = document.createElement('div');
    wrap.className = 'canvas-wrap';
    const canvas = document.createElement('canvas');
    canvas.width = 320;
    canvas.height = 240;
    wrap.append(canvas);
    document.getElementById('terminal')?.before(wrap);
    ctx.onLeave(() => wrap.remove());

    const gl = new GlDevice(canvas);
    if (!gl.ok) {
      ctx.term.writeln(ctx.ansi.red('This browser has no WebGL 2, so there is no GPU to use.'));
      ctx.status('no WebGL 2', 'bad');
      return;
    }

    ctx.term.writeln('Three interlocking gears, drawn by an aarch64 binary.');
    ctx.term.writeln('');
    ctx.term.writeln(ctx.ansi.dim(`  ${gl.describe()}`));
    ctx.term.writeln('');
    ctx.term.writeln(
      ctx.ansi.dim('The guest runs no graphics driver. Mesa would cost tens of thousands of'),
    );
    ctx.term.writeln(
      ctx.ansi.dim('translated instructions per draw and would compile every shader twice, so'),
    );
    ctx.term.writeln(
      ctx.ansi.dim('the split is at the API instead: the guest fills a buffer with commands'),
    );
    ctx.term.writeln(
      ctx.ansi.dim('and hands it over once a frame. The shaders are the ones it wrote.'),
    );
    ctx.term.writeln('');

    const start = async (frames) => {
      ctx.actions([]);
      ctx.status('fetching');
      running(true);
      const t0 = performance.now();
      try {
        const [elf, loader, glesv2, egl] = await Promise.all(
          [
            '/fs/gles-gears',
            '/fs/ld-musl-aarch64.so.1',
            '/fs/libGLESv2.so.2',
            '/fs/libEGL.so.1',
          ].map(async (u) => {
            const r = await fetch(u);
            if (!r.ok) throw new Error(`${u}: ${r.status}`);
            return new Uint8Array(await r.arrayBuffer());
          }),
        );
        rule(ctx, `gles-gears — ${ctx.bytes(elf.length)}`);
        ctx.status('running');
        const { status } = await ctx.runProgram(ctx.term, {
          elf,
          files: {
            '/lib/ld-musl-aarch64.so.1': loader,
            '/usr/lib/libGLESv2.so.2': glesv2,
            '/usr/lib/libEGL.so.1': egl,
          },
          args: ['/gles-gears', String(frames)],
          cwd: '/',
          tty: true,
          gl,
        });
        const ms = (performance.now() - t0).toFixed(0);
        ctx.term.writeln('');
        rule(ctx, `exit ${status} in ${ms} ms`);
        ctx.status(status === 0 ? 'finished' : `exit ${status}`, status === 0 ? 'good' : 'bad');
      } catch (e) {
        failed(ctx, e);
      } finally {
        running(false);
        ctx.idle();
        ctx.actions([{ label: 'Run', primary: true, onClick: () => start(200) }]);
      }
    };

    ctx.actions([{ label: 'Run', primary: true, onClick: () => start(200) }]);
    ctx.hint('every GL call crosses to this page and runs on WebGL');

    // `?frames=N` starts it straight away, so a link can carry the demo.
    const asked = Number(new URLSearchParams(location.search).get('frames'));
    if (Number.isFinite(asked) && asked > 0) {
      await start(Math.min(asked, 2000));
    }
  },
};

// -- 5: the real gears ----------------------------------------------------

const virglGears = {
  id: 'virgl',
  title: 'Debian glxgears, on real Mesa',
  blurb:
    "The unmodified aarch64 `glxgears` Debian ships, against Debian's own Mesa, with an X " +
    'server beside it. Nothing is marshalled at the API: the guest runs a real graphics driver, ' +
    'and what reads its command stream is virglrenderer, compiled to wasm, on this page.',
  network: false,

  async enter(ctx) {
    const wrap = document.createElement('div');
    wrap.className = 'canvas-wrap';
    const canvas = document.createElement('canvas');
    canvas.id = 'gex-virgl';
    canvas.width = 300;
    canvas.height = 300;
    wrap.append(canvas);
    document.getElementById('terminal')?.before(wrap);
    ctx.onLeave(() => wrap.remove());

    ctx.term.writeln('The other GL demo splits at the API: the guest links against libGLESv2');
    ctx.term.writeln('and libEGL that are marshallers, and every call crosses to this page.');
    ctx.term.writeln('');
    ctx.term.writeln(ctx.ansi.dim('This one does not split at all. The guest loads Mesa, Mesa'));
    ctx.term.writeln(ctx.ansi.dim("picks its `virgl` Gallium driver, and that driver writes the"));
    ctx.term.writeln(ctx.ansi.dim('virtio-gpu command stream it would write to a real GPU. The'));
    ctx.term.writeln(ctx.ansi.dim('thing on the other end is virglrenderer -- the same C that runs'));
    ctx.term.writeln(ctx.ansi.dim('on a host, built to WebAssembly, pointed at WebGL 2.'));
    ctx.term.writeln('');
    ctx.term.writeln(ctx.ansi.dim('The guest is not told where its GPU is. It looks for a socket,'));
    ctx.term.writeln(ctx.ansi.dim('asks what the far side can do, and gets an answer from this'));
    ctx.term.writeln(ctx.ansi.dim('browser. On a host that answer is capability set version 2;'));
    ctx.term.writeln(ctx.ansi.dim('here it is version 1, because WebGL 2 is GLES 3.0 and has no'));
    ctx.term.writeln(ctx.ansi.dim('compute or geometry shaders. Mesa falls back to match.'));
    ctx.term.writeln('');
    ctx.term.writeln(
      ctx.ansi.dim('It is slow to start. Every basic block of Mesa, libGL, libX11 and'),
    );
    ctx.term.writeln(
      ctx.ansi.dim('the X server is translated on the way past, and there are a lot.'),
    );
    ctx.term.writeln('');
    ctx.term.writeln(
      ctx.ansi.dim('The first frame is about forty seconds away and the frame rate is'),
    );
    ctx.term.writeln(
      ctx.ansi.dim('around thirty a second after that. glxgears prints its own count'),
    );
    ctx.term.writeln(ctx.ansi.dim('every five seconds; the line at the end says how many of those'));
    ctx.term.writeln(ctx.ansi.dim('frames were actually put on the canvas above.'));
    ctx.term.writeln('');

    // The budget has to clear the cold start: the first frame is ~40s in, so a
    // 30s run ends with a blank canvas and code 124 that reads as a failure.
    const gearsButtons = (go) => [
      { label: 'Run', primary: true, onClick: () => go(300) },
    ];

    const start = async (seconds) => {
      ctx.actions([]);
      running(true);
      const t0 = performance.now();
      let renderer = null;
      try {
        ctx.busy('loading the renderer');
        const { VirglRenderer } = await import('./virgl.js');
        renderer = await VirglRenderer.create(canvas, '/gex/gexvirgl.mjs');
        if (!renderer.ok) throw new Error(`the renderer would not start: ${renderer.error}`);
        // Live render state for a test harness: it can tell a client rendered
        // (lit>0) the moment it happens rather than waiting for the exit report.
        globalThis.__gexGlState = () => {
          try {
            return renderer.glState();
          } catch {
            return '';
          }
        };
        globalThis.__gexGlCommands = () => {
          try {
            return renderer.commands();
          } catch {
            return '';
          }
        };
        // `?virgltrace=1` narrates every vtest command by name, the only way to
        // see what a stuck GL client is looping on.
        if (new URLSearchParams(location.search).get('virgltrace')) renderer.traceCommands(1);
        ctx.term.writeln(ctx.ansi.dim(`  ${renderer.describe().split('|').join('\r\n  ')}`));
        ctx.term.writeln('');

        const tree = await ctx.rootfs('/rootfs/graphics.tar');
        const files = { ...tree.files };
        for (const [path, target] of Object.entries(tree.links)) {
          const abs = rootfsLinkTarget(path, target);
          // A symlink becomes a copy; the mode must travel with the bytes or
      // `/bin/sh` (a symlink to dash) loses its execute bit.
      if (files[abs]) {
        files[path] = files[abs];
        if (tree.modes?.[abs] !== undefined) tree.modes[path] = tree.modes[abs];
      }
    }
    // Hard links resolve against the root of the archive, not the link's
    // directory. `/usr/bin/perl` is one.
    for (const [path, target] of Object.entries(tree.hard ?? {})) {
      if (files[target]) {
        files[path] = files[target];
        if (tree.modes?.[target] !== undefined) tree.modes[path] = tree.modes[target];
      }
        }
        // The loader passes no environment, so the bounds go into a script.
        // `?gears_size=100x100` shrinks the screen, to tell whether a frame
        // costs by the pixel or by the call.
        // `?wlclient=` on the Xvfb (virgl) demo runs that client under Xvfb
        // instead of glxgears -- a control against the Xwayland demo, to tell an
        // Xwayland/wl-specific crash from an environment-independent one.
        const virglClient = new URLSearchParams(location.search).get('wlclient');
        if (virglClient) {
          files['/run.sh'] = new TextEncoder().encode(
            'Xvfb :99 -screen 0 1024x768x24 >/dev/null 2>&1 &\n' +
              'i=0; while [ ! -e /tmp/.X11-unix/X99 ]; do i=$((i+1)); [ $i -gt 30 ] && break; sleep 1; done\n' +
              'export DISPLAY=:99 GSETTINGS_SCHEMA_DIR=/usr/share/glib-2.0/schemas XDG_DATA_DIRS=/usr/share ' +
              'HOME=/root XDG_CACHE_HOME=/tmp GDK_BACKEND=x11 LIBC_FATAL_STDERR_=1\n' +
              'echo "gex: Xvfb up, starting the client"\n' +
              `timeout ${seconds} ${virglClient} 2>&1\n` +
              'echo "gex: client exited $?"\n',
          );
        } else {
          files['/run.sh'] = new TextEncoder().encode(
            `GEARS_SECONDS=${seconds}\nexport GEARS_SECONDS\n` +
              `GEARS_SIZE=${/^\d+x\d+$/.test(new URLSearchParams(location.search).get('gears_size') ?? '') ? new URLSearchParams(location.search).get('gears_size') : '300x300'}\nexport GEARS_SIZE\n` +
              `. /gears.sh\n`,
          );
        }

        ctx.idle();
        rule(ctx, `graphics rootfs — ${Object.keys(files).length} files`);
        ctx.status('running');
        const { status } = await ctx.runProgram(ctx.term, {
          elf: files['/bin/sh'],
          files,
          dirs: tree.dirs,
          modes: tree.modes,
      modes: tree.modes,
          args: ['/bin/sh', '/run.sh'],
          cwd: '/',
          tty: true,
          uid: 0,
          virgl: renderer,
          // `?heartbeat=N` says where the guest is every N safepoints.
          heartbeat: Number(new URLSearchParams(location.search).get('heartbeat')) || 0,
        });
        const ms = (performance.now() - t0).toFixed(0);
        ctx.term.writeln('');
        rule(ctx, `exit ${status} in ${ms} ms`);
        // vtest has no present -- the X server reads its pixels back and never
        // looks at a screen -- so a run can draw perfectly with a blank canvas.
        await renderer.refresh();
        const gl = renderer.glState();
        // `presented` counts blits; `lit` counts blits whose source had a pixel
        // in it. Those differing is a black canvas with the frame reported to
        // have arrived.
        const nums = ['presented', 'lit', 'brightest']
          .map((k) => `${k}=${/=(\d+)/.exec(gl.slice(gl.indexOf(k)))?.[1] ?? '?'}`)
          .join(' ');
        ctx.term.writeln(ctx.ansi.dim(`  ${nums}`));
        ctx.term.writeln(ctx.ansi.dim(`  ${gl}`));
        // The command counts say whether an empty frame had nothing drawn into
        // it or was drawn somewhere else.
        for (const part of renderer.commands().split('|')) {
          ctx.term.writeln(ctx.ansi.dim(`  ${part.trim()}`));
        }
        ctx.status(status === 0 ? 'finished' : `exit ${status}`, status === 0 ? 'good' : 'bad');
      } catch (e) {
        failed(ctx, e);
      } finally {
        renderer?.dispose();
        running(false);
        ctx.idle();
        ctx.actions(gearsButtons(start));
      }
    };

    ctx.actions(gearsButtons(start));
    ctx.hint('Xvfb and glxgears, as Debian built them');
  },
};

// The graphical clients the Wayland demo can launch, chosen from its dropdown.
// Each runs against the one gex-wayland compositor (in the shim); `mode` is how:
//   'x'   — an X client under Xwayland (OpenGL through virgl)
//   'gl'  — a native Wayland OpenGL client (EGL's wl_shm/drisw path, GL via virgl)
//   'shm' — a native Wayland *toolkit* client that draws in software into a
//           wl_shm buffer (GTK's wayland backend, Qt's libqwayland-generic): no
//           OpenGL at all, so it exercises the compositor and the toolkit rather
//           than Mesa. These are the real Qt/GTK/KDE apps.
const WL_TOOLKIT_ENV = {
  qt6:
    'export QT_QPA_PLATFORM=wayland\n' +
    'export QT_PLUGIN_PATH=/usr/lib/aarch64-linux-gnu/qt6/plugins\n' +
    'export FONTCONFIG_PATH=/etc/fonts\n' +
    'export DBUS_SESSION_BUS_ADDRESS=disabled:\n' +
    'export QT_LOGGING_RULES="*=false"\n',
  qt5:
    'export QT_QPA_PLATFORM=wayland\n' +
    'export QT_PLUGIN_PATH=/usr/lib/aarch64-linux-gnu/qt5/plugins\n' +
    'export FONTCONFIG_PATH=/etc/fonts\n' +
    'export DBUS_SESSION_BUS_ADDRESS=disabled:\n' +
    'export QT_LOGGING_RULES="*=false"\n',
  // The base env forces GDK_BACKEND=x11 for the Xwayland clients; put it back to
  // wayland and point the pixbuf loaders at the cache baked into the rootfs.
  gtk:
    'export GDK_BACKEND=wayland\n' +
    'export GDK_PIXBUF_MODULE_FILE=/usr/lib/aarch64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache\n',
};

const WL_APPS = [
  { id: 'glxgears', label: 'glxgears — OpenGL via virgl (Xwayland)', mode: 'x', client: 'glxgears.aarch64-linux-gnu -info' },
  { id: 'es2gears', label: 'es2gears — OpenGL ES, native Wayland', mode: 'gl', client: '/usr/bin/es2gears_wayland.aarch64-linux-gnu' },
  { id: 'gtk3-demo', label: 'GTK 3 — Demo', mode: 'shm', client: 'gtk3-demo', env: WL_TOOLKIT_ENV.gtk },
  { id: 'gtk3-widget-factory', label: 'GTK 3 — Widget Factory', mode: 'shm', client: 'gtk3-widget-factory', env: WL_TOOLKIT_ENV.gtk },
  { id: 'qt6ct', label: 'Qt 6 — Configuration Tool', mode: 'shm', client: 'qt6ct', env: WL_TOOLKIT_ENV.qt6 },
  { id: 'explosive-c4', label: 'Qt 6 — Explosive C4 (game)', mode: 'shm', client: 'explosive-c4', env: WL_TOOLKIT_ENV.qt6 },
  { id: 'compton-conf', label: 'Qt 5 — compton-conf', mode: 'shm', client: 'compton-conf', env: WL_TOOLKIT_ENV.qt5 },
  { id: 'kcalc', label: 'KDE / KF5 — KCalc', mode: 'shm', client: 'kcalc', env: WL_TOOLKIT_ENV.qt5 },
  {
    id: 'dolphin',
    label: 'KDE / KF5 — Dolphin (file manager)',
    mode: 'shm',
    client: 'dolphin --new-window /root',
    // Qt5 base env plus a writable config home: Dolphin's first run saves
    // bookmarks (user-places.xbel) and view state via QSaveFile's O_TMPFILE +
    // linkat atomic-write into $XDG_CONFIG_HOME, so it must point at a writable
    // dir rather than defaulting under a read-only path.
    env: WL_TOOLKIT_ENV.qt5 + 'export XDG_CONFIG_HOME=/root/.config\n',
    // A profile-guided AOT closure (built by web/build-closure.sh from a
    // reached-PC coverage run to first frame). The dispatch crash that used to
    // hit every graphics-rootfs wayland closure (libc+0x84710) is fixed by
    // `closureBuildNumbered` (see runProgram), so Dolphin now renders in a real
    // browser.
    closure: '/closure/dolphin',
    closureImage: '/bin/sh',
    // Drop Mesa's software rasteriser from Dolphin's filesystem. Dolphin is a
    // QtWidgets app -- it renders through Qt's own raster engine into wl_shm and
    // needs no GL -- but on startup something (a KDE/EGL probe) dlopens
    // swrast_dri.so, whose mappings pushed the guest's 32-bit (4 GiB) wasm memory
    // over the top and crashed the tab. Absent, the probe fails gracefully and
    // Dolphin renders under the ceiling. Scoped to this app: glxgears' Xwayland
    // GLX genuinely needs swrast, and it does not drop it.
    dropFiles: ['/usr/lib/aarch64-linux-gnu/dri/swrast_dri.so'],
  },
];

// Which app to start with: an explicit `?wlclient=<id>` (the drive.mjs harness
// uses it) wins, then the dropdown. A `?wlclient=` that is a raw command rather
// than a known id still runs, as whatever `?wlmode` says (default Xwayland).
function pickInitialApp(qp) {
  const wanted = qp.get('wlclient');
  if (wanted) {
    const known = WL_APPS.find((a) => a.id === wanted);
    if (known) return known;
    return { id: wanted, label: wanted, client: wanted, mode: qp.get('wlmode') === 'native' ? 'gl' : 'x' };
  }
  return WL_APPS[0];
}

// XWayland/Wayland, skipping Xvfb: gex-wayland (a compositor in the shim)
// binds `/tmp/wayland-0`, a client connects, and its committed shm frames are
// blitted onto a 2D canvas here. The dropdown picks the client: real Qt/GTK/KDE
// toolkit apps (native wl_shm), a native Wayland GL client, or glxgears under
// Xwayland.
const waylandGears = {
  id: 'wayland',
  title: 'Qt / GTK / KDE apps on gex-wayland',
  blurb:
    'A gex-native Wayland compositor lives in the shim; a client connects and its committed shm ' +
    'frames are blitted onto a canvas here. Pick a client from the dropdown: real Qt, GTK and KDE ' +
    'apps draw in software into wl_shm, or glxgears/es2gears render OpenGL through virgl.',
  network: false,

  async enter(ctx) {
    const demo = this;
    const wrap = document.createElement('div');
    wrap.className = 'canvas-wrap';
    // The Wayland compositor's output -- the one window the user sees. `serveWl`
    // resizes it to each committed frame (so it takes the app's real size), and
    // `attachWlInput` routes mouse+keyboard to it; `tabIndex` makes it able to
    // take keyboard focus (a bare <canvas> cannot). Every client shows here:
    // toolkit apps commit software frames, and the GL clients' pixels are read
    // back into wl_shm and composited here too.
    const wlCanvas = document.createElement('canvas');
    wlCanvas.id = 'gex-wl';
    wlCanvas.width = 640;
    wlCanvas.height = 480;
    wlCanvas.tabIndex = 0;
    wrap.append(wlCanvas);
    // virglrenderer's own GL surface, a WebGL context the GL clients execute on.
    // Which canvas the user actually sees depends on the client (see
    // `showWindow`): glxgears under Xwayland presents *here*, while toolkit apps
    // and the native-GL client present on `wlCanvas`. The other is moved
    // off-screen (kept full-size and rendering) so only one window ever shows.
    const canvas = document.createElement('canvas');
    canvas.width = demo.canvasWidth || 300;
    canvas.height = demo.canvasHeight || 300;
    wrap.append(canvas);
    const fps = document.createElement('output');
    fps.className = 'fps';
    fps.textContent = 'FPS: —';
    wrap.append(fps);
    document.getElementById('terminal')?.before(wrap);
    let fpsTimer = 0;
    let fpsRaf = 0;
    ctx.onLeave(() => {
      clearInterval(fpsTimer);
      cancelAnimationFrame(fpsRaf);
      wrap.remove();
    });

    // Show exactly one window: the WebGL surface for an Xwayland GL client
    // ('x'), the compositor's output for everything else ('gl'/'shm').
    const showWindow = (a) => {
      const glWindow = a.mode === 'x';
      canvas.classList.toggle('wl-offscreen', !glWindow);
      wlCanvas.classList.toggle('wl-offscreen', glWindow);
      fps.hidden = a.mode === 'shm';
    };

    // A demo that names its own client (SuperTuxKart) runs just that; otherwise
    // an app-picker dropdown chooses among the Qt/GTK/KDE and GL clients. The
    // selection is what `start` launches.
    const qp0 = new URLSearchParams(location.search);
    let app = demo.client
      ? { id: demo.id, label: demo.title, client: qp0.get('wlclient') || demo.client, mode: demo.mode || 'x', env: '' }
      : pickInitialApp(qp0);
    if (!demo.client) {
      // Kept in the demo's own DOM (cleaned up on leave) rather than the shared
      // #actions bar, which `ctx.actions` replaces wholesale.
      const bar = document.createElement('div');
      bar.className = 'wl-picker';
      const label = document.createElement('label');
      label.textContent = 'App: ';
      const select = document.createElement('select');
      for (const a of WL_APPS) {
        const opt = document.createElement('option');
        opt.value = a.id;
        opt.textContent = a.label;
        select.append(opt);
      }
      // A `?wlclient=` that is a raw command, not a listed id, gets its own
      // option so the dropdown still shows what is selected.
      if (!WL_APPS.some((a) => a.id === app.id)) {
        const opt = document.createElement('option');
        opt.value = app.id;
        opt.textContent = app.label;
        select.append(opt);
      }
      select.value = app.id;
      label.append(select);
      bar.append(label);
      wrap.before(bar);
      ctx.onLeave(() => bar.remove());
      select.onchange = () => {
        app = WL_APPS.find((a) => a.id === select.value) || app;
        ctx.hint(app.label);
        showWindow(app);
      };
    }
    showWindow(app);

    const intro = demo.intro || [
      'Xwayland replaces Xvfb. It connects to a Wayland compositor that lives in',
      'the shim, and glxgears renders through virgl -- real Mesa, no software',
      'rasteriser -- to virglrenderer on this page.',
    ];
    for (const line of intro) ctx.term.writeln(line);
    ctx.term.writeln('');

    const start = async (seconds, runOptions = {}) => {
      ctx.actions([]);
      running(true);
      const t0 = performance.now();
      let renderer = null;
      try {
        ctx.busy('loading the renderer');
        const { VirglRenderer } = await import('./virgl.js');
        renderer = await VirglRenderer.create(canvas, '/gex/gexvirgl.mjs');
        if (!renderer.ok) throw new Error(`the renderer would not start: ${renderer.error}`);
        clearInterval(fpsTimer);
        cancelAnimationFrame(fpsRaf);
        fps.textContent = 'FPS: 0.0';
        let fpsFrames = 0;
        let fpsSerial = renderer.presentedCount();
        let fpsAt = performance.now();
        const countDisplayedFrame = () => {
          const serial = renderer.presentedCount();
          if (serial !== fpsSerial) {
            fpsFrames++;
            fpsSerial = serial;
          }
          fpsRaf = requestAnimationFrame(countDisplayedFrame);
        };
        fpsRaf = requestAnimationFrame(countDisplayedFrame);
        fpsTimer = setInterval(() => {
          const now = performance.now();
          fps.textContent = `FPS: ${(fpsFrames * 1000 / (now - fpsAt)).toFixed(1)}`;
          fpsFrames = 0;
          fpsAt = now;
        }, 1000);
        // Live render state for a test harness: it can tell a client rendered
        // (lit>0) the moment it happens rather than waiting for the exit report.
        globalThis.__gexGlState = () => {
          try {
            return renderer.glState();
          } catch {
            return '';
          }
        };
        globalThis.__gexGlCommands = () => {
          try {
            return renderer.commands();
          } catch {
            return '';
          }
        };
        // `?virgltrace=1` narrates every vtest command by name, the only way to
        // see what a stuck GL client is looping on.
        if (new URLSearchParams(location.search).get('virgltrace')) renderer.traceCommands(1);
        ctx.term.writeln(ctx.ansi.dim(`  ${renderer.describe().split('|').join('\r\n  ')}`));
        ctx.term.writeln('');

        const qp = new URLSearchParams(location.search);
        // Keep the stock graphics image as the default, but let heavier X11
        // applications bring their own traced/minimised rootfs through the
        // same Xwayland + Wayland + virgl pipeline. The URL is still fetched
        // from this site's rootfs directory; only the asset name changes.
        const rootfsUrl = qp.get('wlrootfs') || demo.rootfs || '/rootfs/graphics.tar';
        const tree = await ctx.rootfs(rootfsUrl);
        const files = { ...tree.files };
        for (const [path, target] of Object.entries(tree.links)) {
          const abs = rootfsLinkTarget(path, target);
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
        // `?wlclient=` overrides the X client (default glxgears); `?wlenv=a=b;c=d`
        // adds env, so a probe needs no rebuild. GALLIUM_DRIVER=virpipe is the
        // whole point: glxgears' GL goes through Mesa's virgl driver to the vtest
        // socket, not a software rasteriser.
        const extraEnv = [demo.env, qp.get('wlenv')]
          .filter(Boolean)
          .join(';')
          .split(';')
          .filter(Boolean)
          .map((e) => `export ${e}\n`)
          .join('');
        const baseEnv =
          'export XDG_RUNTIME_DIR=/tmp\n' +
          'export WAYLAND_DISPLAY=wayland-0\n' +
          'export LD_LIBRARY_PATH=/usr/lib/aarch64-linux-gnu\n' +
          // A home and a writable cache so fontconfig (xclock, xterm, any Xft
          // client) can find fonts and write its scan cache rather than failing.
          'export HOME=/root\n' +
          'export XDG_CACHE_HOME=/tmp\n' +
          // A shell and a terminal type, for xterm (which forks $SHELL on a pty).
          'export SHELL=/bin/sh\n' +
          'export TERM=xterm\n' +
          // GTK: force the X11 backend (run under Xwayland like the other X
          // clients), point it at the compiled GSettings schemas and the icon/
          // mime data, and give it a writable pixbuf loader cache.
          'export GDK_BACKEND=x11\n' +
          'export GSETTINGS_SCHEMA_DIR=/usr/share/glib-2.0/schemas\n' +
          'export XDG_DATA_DIRS=/usr/share\n' +
          'export GDK_PIXBUF_MODULE_FILE=/tmp/gdk-loaders.cache\n' +
          // Tcl/Tk's script libraries (wish loads init.tcl/tk.tcl from here).
          'export TCL_LIBRARY=/usr/share/tcltk/tcl8.6\n' +
          'export TK_LIBRARY=/usr/share/tcltk/tk8.6\n' +
          'export GALLIUM_DRIVER=virpipe\n' +
          // Mesa's async shader-disk-cache runs a WORKER THREAD -- a second
          // Worker on the process's memory. Two robustness fixes made real
          // progress here (the precise cross-Worker futex wake k_futex_wake so
          // its condvar signal is not lost, and the compare-and-swap in the
          // arena allocator so two Workers claiming memory at once cannot be
          // handed the same bytes), but a residual hazard still crashes the
          // second Mesa thread, so the cache is disabled by default and the
          // demos run single-threaded. `?wlenv=MESA_SHADER_CACHE_DISABLE=false`
          // turns it back on to exercise/diagnose the multithreaded path.
          'export MESA_SHADER_CACHE_DISABLE=true\n' +
          extraEnv;
        // Keeping `timeout` as the normal demo guard is useful, but profiling
        // clients often exit on their own. Running those directly avoids an
        // extra process whose wait state can hide the client's real exit from
        // the browser harness.
        const runClient = (client) =>
          runOptions.noTimeout || qp.has('wlnotimeout')
            ? client
            : `timeout ${seconds} ${client}`;
        // `app.client` already resolves `?wlclient=` (via `pickInitialApp`, or
        // `demo.client` below); a per-action `runOptions.client` (the
        // SuperTuxKart "Play" preset) is the only extra override.
        const client = runOptions.client || app.client;
        const run =
          `echo "gex: running ${client}"\n` +
          `${runClient(client)} 2>&1\n` +
          'echo "gex: client exited $?"\n';
        let script;
        if (app.mode === 'shm') {
          // A native Wayland toolkit client (GTK's wayland backend, Qt's
          // libqwayland-generic) drawing in software into wl_shm -- no X, no GL.
          // The app's own env (QT_QPA_PLATFORM=wayland, GDK_BACKEND=wayland, the
          // plugin paths) goes on top of the base env.
          script = baseEnv + (app.env || '') + run;
        } else if (app.mode === 'gl') {
          // A wayland-native GL client straight against gex-wayland -- no X at
          // all. GL still goes through virgl (virpipe) to virglrenderer; EGL's
          // wl_shm/drisw path needs no gbm/DRM and no libLLVM.
          // EGL_PLATFORM=wayland is what makes Mesa's EGL take the wl_shm/drisw
          // path (→ virgl via virpipe) instead of probing for a GPU/gbm device
          // it cannot find. Without it eglInitialize fails; with it, it comes up
          // and enumerates real configs.
          script = baseEnv + 'export EGL_PLATFORM=wayland\n' + (app.env || '') + run;
        } else {
          // Xwayland as the X server, glxgears (or a raw ?wlclient=/demo.client)
          // as an X client.
          script =
            baseEnv +
            (app.env || '') +
            'echo "gex: starting Xwayland"\n' +
            // `-ac` disables access control. Xwayland turns authorization on by
            // default and there is no `.Xauthority` in this rootfs, so a client
            // is refused with "Authorization required, but no authorization
            // protocol specified" and glxgears cannot open display :0. Xvfb was
            // reached without it because that path never enabled auth.
            'Xwayland :0 -verbose 3 -noreset -ac &\n' +
            // Do NOT poll for /tmp/.X11-unix/X0: Xwayland binds it in its own
            // process, and with a Worker (and a tmpfs) per process the node it
            // makes is not in this shell's filesystem -- the parent would wait
            // forever for a path only the child can see. The socket *name*,
            // though, is registered in the global kernel (the abstract
            // @/tmp/.X11-unix/X0 that libxcb tries first), so a client connects
            // across the process boundary regardless. Xwayland listens the
            // instant it binds, long before it finishes bringing extensions up,
            // so a fixed spell to let it bind is enough and the client's
            // connection is queued until the server is ready.
            'sleep 6\n' +
            'export DISPLAY=:0\n' +
            // No `gdk-pixbuf-query-loaders` here: it hangs this pipeline (a
            // forked child that never returns, so the shell's `wait` blocks and
            // the client never starts), and it could not help anyway -- it writes
            // its cache into THIS shell's per-process tmpfs, which a forked client
            // does not share, so a GTK client would never see it. A GTK-in-wayland
            // demo that needs the loader cache must build it a way that crosses
            // the process boundary (a shared tmpfs, or bake the cache into the
            // rootfs at build time).
            'echo "gex: X up, starting the client"\n' +
            run;
        }
        files['/run.sh'] = new TextEncoder().encode(script);
        // Files this app must not see (e.g. Dolphin drops Mesa's swrast so its
        // guest memory stays under the 4 GiB wasm ceiling). Other apps that
        // share this rootfs keep them.
        for (const path of app.dropFiles || []) delete files[path];

        // A per-app AOT closure, when one has been built (Dolphin). Loaded like
        // the debian demo's: `loadClosure` answers null when there is no
        // manifest, so trying costs nothing, and `?closure=0` forces the
        // runtime translator. pid 1 is `/bin/sh`, so the manifest is checked
        // against that image. Without it Dolphin's Qt5/KF5/Mesa is lifted from
        // scratch -- ~100s to first frame and a >7 GiB peak that OOMs the tab.
        // `?appclosure=/closure/<name>` overrides which closure to load (for
        // bisecting the closure path against a known-good one); `?closure=0`
        // forces the runtime translator.
        const qpc = new URLSearchParams(location.search);
        const closurePath = qpc.get('appclosure') || app.closure;
        let appClosure = null;
        if (closurePath && qpc.get('closure') !== '0') {
          ctx.busy('loading the closure');
          const { loadClosure } = await import('./closure.js');
          try {
            appClosure = await loadClosure(closurePath, {
              image: app.closureImage || '/bin/sh',
              log: (s) => console.debug(`[gex] ${s}`),
            });
          } catch (e) {
            console.debug(`[gex] no ${app.id} closure: ${e.message}`);
          }
        }

        ctx.idle();
        rule(ctx, `graphics rootfs — ${Object.keys(files).length} files`);
        if (appClosure) {
          ctx.term.writeln(
            ctx.ansi.dim(`closure: ${appClosure.libs.length} libraries, translated ahead of time`),
          );
        }
        ctx.status('running');
        const { status } = await ctx.runProgram(ctx.term, {
          elf: files['/bin/sh'],
          files,
          dirs: tree.dirs,
          modes: tree.modes,
          args: ['/bin/sh', '/run.sh'],
          cwd: '/',
          tty: true,
          uid: 0,
          closure: appClosure,
          // Number the closure's slots at the build's numbers. This demo runs a
          // single client, so the per-process demand allocator's small-table win
          // does not matter, and its streaming interaction corrupts dispatch for
          // a profile-guided closure whose pages are demand-filled while pid 1 is
          // already running (the libc+0x84710 trap). See runProgram.
          closureBuildNumbered: !!appClosure,
          // Real GL over virgl, as the glxgears demo does. The compositor gets
          // a 2D canvas so a native wl client's committed shm frames show; for
          // the Xwayland path the pixels come via virgl and this stays blank.
          virgl: renderer,
          wl: wlCanvas,
          // Focus and input go to the one visible canvas: the virgl/WebGL one
          // for an Xwayland client ('x'), the compositor's output otherwise.
          wlInput: app.mode === 'x' ? canvas : wlCanvas,
          heartbeat: Number(new URLSearchParams(location.search).get('heartbeat')) || 0,
        });
        const ms = (performance.now() - t0).toFixed(0);
        ctx.term.writeln('');
        rule(ctx, `exit ${status} in ${ms} ms`);
        await renderer.refresh();
        const gl = renderer.glState();
        const nums = ['presented', 'lit', 'brightest']
          .map((k) => `${k}=${/=(\d+)/.exec(gl.slice(gl.indexOf(k)))?.[1] ?? '?'}`)
          .join(' ');
        ctx.term.writeln(ctx.ansi.dim(`  ${nums}`));
        ctx.term.writeln(ctx.ansi.dim(`  ${gl}`));
        for (const part of renderer.commands().split('|')) {
          ctx.term.writeln(ctx.ansi.dim(`  ${part.trim()}`));
        }
        ctx.status(status === 0 ? 'finished' : `exit ${status}`, status === 0 ? 'good' : 'bad');
      } catch (e) {
        failed(ctx, e);
      } finally {
        clearInterval(fpsTimer);
        cancelAnimationFrame(fpsRaf);
        fpsTimer = 0;
        fpsRaf = 0;
        renderer?.dispose();
        running(false);
        ctx.idle();
        ctx.actions(demo.actions?.(start) || [
          { label: 'Run', primary: true, onClick: () => start(300) },
        ]);
      }
    };

    ctx.actions(demo.actions?.(start) || [
      { label: 'Run', primary: true, onClick: () => start(300) },
    ]);
    ctx.hint(demo.hint || app.label);
  },
};

const stkVideoOptions =
  '--screensize=640x480 --windowed --disable-hd-textures --disable-dynamic-lights ' +
  '--disable-glow --disable-bloom --disable-light-shaft --disable-dof --disable-particles ' +
  '--disable-animated-characters --disable-motion-blur --disable-mlaa --disable-ssao ' +
  '--disable-ibl --shadows=0 --anisotropic=0';
const stkRace =
  `/usr/games/supertuxkart --race-now --track=snowtuxpeak --kart=tux ` +
  `--numkarts=1 --laps=1 ${stkVideoOptions}`;
const stkPlay = `/usr/games/supertuxkart ${stkVideoOptions}`;
const stkProfileValue = Number(new URLSearchParams(location.search).get('stkprofile'));
// Preserve the original `?stkprofile=1` harness shorthand (a five-second
// profile), while accepting an explicit duration for longer stable samples.
const stkProfileSeconds = stkProfileValue === 1 ? 5 : Math.floor(stkProfileValue);
const stkLaunch = stkProfileSeconds > 0
  ? `${stkRace} --profile-time=${Math.min(300, stkProfileSeconds)}`
  : stkPlay;

// The real application workload for the complete browser graphics pipeline.
// It shares the Wayland implementation above, but carries its own rootfs and
// launch presets so selecting the card needs no query-string incantation.
const supertuxkart = {
  id: 'supertuxkart',
  title: 'SuperTuxKart on Xwayland',
  blurb:
    'Run the AArch64 SuperTuxKart package through gex-wayland, Xwayland, Mesa virgl, and ' +
    'virglrenderer into WebGL in this browser.',
  network: false,
  // Match STK's framebuffer, Xwayland's advertised output, and the displayed
  // canvas one-for-one instead of rendering tiny and stretching in CSS.
  canvasWidth: 640,
  canvasHeight: 480,
  // Version the generated image so a persistent browser does not reuse an
  // earlier over-trimmed tar that lacked the track and kart materials.
  rootfs: '/rootfs/stk.tar?v=snowpeak18',
  env:
    'IRR_DEVICE_TYPE=x11;SUPERTUXKART_DATADIR=/usr/share/games/supertuxkart;' +
    'ALSOFT_DRIVERS=null;MESA_GLSL_CACHE_DISABLE=1;MESA_GL_VERSION_OVERRIDE=3.3;' +
    'MESA_GLSL_VERSION_OVERRIDE=330;JSIMD_FORCENONE=1;' +
    'LD_PRELOAD=/usr/lib/aarch64-linux-gnu/libgex-stk-sdl.so',
  // Auto-run is playable by default. The harness adds ?stkprofile=1 for its
  // finite benchmark run without exposing profiling controls in the demo UI.
  client: stkLaunch,
  intro: [
    'SuperTuxKart is running as an AArch64 X11 application inside this page:',
    'Xwayland -> gex-wayland, Mesa virgl -> virglrenderer -> browser WebGL.',
  ],
  hint: 'SuperTuxKart, Xwayland, and Mesa virgl in a real browser',
  actions: (start) => [
    { label: 'Run', primary: true, onClick: () => start(300, { client: stkLaunch }) },
  ],
  enter: waylandGears.enter,
};

export const DEMOS = [
  upload,
  busybox,
  debian,
  gears,
  virglGears,
  waylandGears,
  supertuxkart,
];
