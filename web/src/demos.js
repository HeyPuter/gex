// The demos. Each has an `enter(ctx)` that owns the terminal until the next is
// chosen; `ctx` carries the page's chrome so a demo never touches the DOM.

import { archAsset, multiarch, elfMachine, guestArch } from './runtime.js';

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
      if (buf[18] !== elfMachine()) {
        ctx.term.writeln(
          ctx.ansi.red(`${file.name} is not ${guestArch()} (e_machine=${buf[18] | (buf[19] << 8)})`),
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
    // `?image=busybox32` boots the i386 image on the x86 guest: the same
    // busybox as a 32-bit program, run in compatibility mode. Its closure is
    // the 64-bit one's, so none is loaded for it.
    const image = new URLSearchParams(location.search).get('image');
    const tree = await ctx.rootfs(archAsset(image ? `/rootfs/${image}.tar` : '/rootfs/busybox.tar'));
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
    const wantClosure = new URLSearchParams(location.search).get('closure') !== '0' && !image;
    let closure = null;
    try {
      closure = !wantClosure ? null : await loadClosure(archAsset('/closure/busybox'), {
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
    const tree = await ctx.rootfs(archAsset('/rootfs/debian.tar'));
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
    const closure = !wantClosure ? null : await loadClosure(archAsset('/closure/debian'), {
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

/** The Mesa device's Gallium driver and module: st/mesa on gex_wgpu. */
function mesaBackend(query, api) {
  return {
    backend: 'wgpu',
    url: '/gex/gex-wgpu-replay.js',
    api: query.get('glapi') || api,
    major: Number(query.get('glmajor') || 0),
    minor: Number(query.get('glminor') || 0),
  };
}

// -- 4: the gears ---------------------------------------------------------

const gears = {
  id: 'gears',
  title: 'OpenGL, on your GPU',
  blurb:
    'An ordinary OpenGL ES 2 program. It includes the standard headers and links against ' +
    'libEGL and libGLESv2 by their usual names -- and those libraries, inside the guest, are ' +
    'marshallers: every call crosses to this page and runs on Mesa (gex_wgpu) over WebGPU.',
  network: false,

  async enter(ctx) {
    const query = new URLSearchParams(location.search);

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

    // st/mesa and the gex_wgpu Gallium driver below the decoder: the only
    // browser GL backend (there is no WebGL path).
    const gl = await (await import('./mesa-gl.js')).MesaGlDevice.create(
      canvas, mesaBackend(query, 'es', 'wgpu'));
    if (!gl.ok) {
      ctx.term.writeln(ctx.ansi.red(gl.error || 'This browser has no usable GPU API.'));
      ctx.status('no GPU', 'bad');
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

    // Nothing reaches the canvas until someone asks for it.
    //
    // The backend renders into an offscreen texture and copies it to the
    // canvas surface only when `display()` is called; the guest's swap only
    // marks a frame produced. The wayland demo below drives that from its own
    // animation loop, and this demo -- whose entire point is to show a
    // picture -- had none, so its canvas was black for as long as it ran on
    // Mesa. The terminal said "frames 200" and the gate matched on that text,
    // so nothing noticed.
    let displayRaf = 0;
    const pump = () => {
      gl.display?.();
      displayRaf = requestAnimationFrame(pump);
    };
    const stopPump = () => {
      if (displayRaf) cancelAnimationFrame(displayRaf);
      displayRaf = 0;
    };
    ctx.onLeave(stopPump);

    const start = async (frames) => {
      ctx.actions([]);
      ctx.status('fetching');
      running(true);
      if (!displayRaf) displayRaf = requestAnimationFrame(pump);
      const t0 = performance.now();
      try {
        const qp = new URLSearchParams(location.search);
        const guestProgram = qp.get('glclient') || '/fs/gles-gears';
        // Repeated `?glarg=` values let the test harness launch an arbitrary
        // unmodified EGL/OpenGL binary (notably Khronos glcts) through this
        // same loader. The gears demo keeps its ordinary frame-count argument.
        const guestArgs = qp.has('glarg') ? qp.getAll('glarg') : [String(frames)];
        // `?glenv=KEY=VALUE`, repeatable, for the guest's own diagnostics --
        // GEXGL_EXTENSION_TRACE most of all, which lists the host extensions
        // the guest declines to repeat. Without this the flag existed and
        // could not be set: nothing between tests/gl44-probe.sh and the
        // guest process carried an environment. Passing any of these replaces
        // the shim's default environment, which a GL probe does not need.
        const guestEnv = qp.getAll('glenv');
        const [elf, loader, glesv2, egl] = await Promise.all(
          [
            guestProgram,
            guestArch() === 'x86' ? '/fs/ld-musl-x86_64.so.1' : '/fs/ld-musl-aarch64.so.1',
            '/fs/libGLESv2.so.2',
            '/fs/libEGL.so.1',
          ].map(archAsset).map(async (u) => {
            // These are development/build artifacts under stable filesystem
            // names. A persistent CDP browser must not keep yesterday's guest
            // driver after the server has rebuilt it.
            const r = await fetch(u, { cache: 'no-store' });
            if (!r.ok) throw new Error(`${u}: ${r.status}`);
            return new Uint8Array(await r.arrayBuffer());
          }),
        );
        const extraFiles = {};
        await Promise.all(
          qp.getAll('glfile').map(async (u) => {
            const r = await fetch(u, { cache: 'no-store' });
            if (!r.ok) throw new Error(`${u}: ${r.status}`);
            extraFiles[u] = new Uint8Array(await r.arrayBuffer());
          }),
        );
        rule(ctx, `${guestProgram.split('/').pop()} — ${ctx.bytes(elf.length)}`);
        ctx.status('running');
        const { status } = await ctx.runProgram(ctx.term, {
          elf,
          files: {
            ...extraFiles,
            [guestArch() === 'x86' ? '/lib/ld-musl-x86_64.so.1' : '/lib/ld-musl-aarch64.so.1']: loader,
            // CTS and ordinary desktop loaders ask for the unversioned link
            // names with dlopen. They are the same driver bytes here; the VFS
            // has no symlink object in this small directly bundled filesystem.
            '/usr/lib/libGLESv2.so': glesv2,
            '/usr/lib/libGLESv2.so.2': glesv2,
            '/usr/lib/libEGL.so': egl,
            '/usr/lib/libEGL.so.1': egl,
          },
          args: [guestProgram, ...guestArgs],
          env: guestEnv,
          cwd: '/',
          tty: true,
          gl,
        });
        // Once more now the guest has exited: its last swapped frame is
        // produced but not yet copied, and the loop above stops with it. A
        // short run would otherwise leave the canvas on an earlier frame, or
        // on nothing at all.
        gl.display?.();
        stopPump();
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
    ctx.hint('every GL call crosses to this page and runs on Mesa over WebGPU');

    // `?frames=N` starts it straight away, so a link can carry the demo.
    const asked = Number(new URLSearchParams(location.search).get('frames'));
    if (Number.isFinite(asked) && asked > 0) {
      await start(Math.min(asked, 2000));
    }
  },
};

// The graphical clients the Wayland demo can launch, chosen from its dropdown.
// Each runs against the one gex-wayland compositor (in the shim); `mode` is how:
//   'x'   — an X client under Xwayland
//   'gl'  — a native Wayland OpenGL client (EGL's wl_shm/drisw path)
//   'shm' — a native Wayland *toolkit* client that draws in software into a
//           wl_shm buffer (GTK's wayland backend, Qt's libqwayland-generic): no
//           OpenGL at all, so it exercises the compositor and the toolkit rather
//           than Mesa. These are the real Qt/GTK/KDE apps.
const WL_TOOLKIT_ENV = {
  qt6:
    'export QT_QPA_PLATFORM=wayland\n' +
    `export QT_PLUGIN_PATH=/usr/lib/${multiarch()}/qt6/plugins\n` +
    'export FONTCONFIG_PATH=/etc/fonts\n' +
    'export DBUS_SESSION_BUS_ADDRESS=disabled:\n' +
    'export QT_LOGGING_RULES="*=false"\n',
  qt5:
    'export QT_QPA_PLATFORM=wayland\n' +
    `export QT_PLUGIN_PATH=/usr/lib/${multiarch()}/qt5/plugins\n` +
    'export FONTCONFIG_PATH=/etc/fonts\n' +
    'export DBUS_SESSION_BUS_ADDRESS=disabled:\n' +
    'export QT_LOGGING_RULES="*=false"\n',
  // The base env forces GDK_BACKEND=x11 for the Xwayland clients; put it back to
  // wayland and point the pixbuf loaders at the cache baked into the rootfs.
  gtk:
    'export GDK_BACKEND=wayland\n' +
    `export GDK_PIXBUF_MODULE_FILE=/usr/lib/${multiarch()}/gdk-pixbuf-2.0/2.10.0/loaders.cache\n`,
};

// A listed client is a string or, when its name carries the multiarch tuple, a
// function of the guest arch (see WL_APPS).
const resolveClient = (c) => (typeof c === 'function' ? c() : c);

const WL_APPS = [
  // Functions, not strings: `multiarch()` depends on `?arch=`, which is parsed
  // after this module is evaluated, so the name is chosen when the app runs.
  { id: 'glxgears', label: 'glxgears — OpenGL (Xwayland)', mode: 'x', client: () => `glxgears.${multiarch()} -info` },
  { id: 'es2gears', label: 'es2gears — OpenGL ES, native Wayland', mode: 'gl', client: () => `/usr/bin/es2gears_wayland.${multiarch()}` },
  // The same program built for X11, which is the one that runs on gex-gl:
  // `?gl=mesa` replaces EGL with this project's marshaller, and that has no
  // Wayland platform -- no wl_egl_window, no wl_buffer -- so the native
  // Wayland build renders one frame and then waits forever for a frame
  // callback that cannot arrive. Under Xwayland the same binary presents on
  // the GL canvas, which is the path SuperTuxKart uses.
  { id: 'es2gears-x11', label: 'es2gears — OpenGL ES on X11 (Xwayland)', mode: 'x', client: () => `/usr/bin/es2gears_x11.${multiarch()}` },
  // The smallest real GL program here, and the one that covers client-side
  // vertex arrays: it points glVertexAttribPointer straight at a stack array
  // with no buffer bound, which is how most tutorial code and every
  // mesa-demos program feeds geometry.
  { id: 'es2tri', label: 'es2tri — a triangle, client-side arrays', mode: 'x', client: () => `/usr/bin/es2tri.${multiarch()}` },
  // The heaviest thing here, and the only one that arrives through GLX rather
  // than EGL: thirty shader-based scenes, which is why it finds driver bugs
  // the small demos cannot. Its contexts are legacy ones, so it runs in a
  // compatibility profile -- see gexgl-glx.inc.
  { id: 'glmark2', label: 'glmark2 — GL benchmark suite (GLX)', mode: 'x', client: 'glmark2 -b :duration=2' },
  { id: 'glxinfo', label: 'glxinfo — GLX/GL capability dump', mode: 'x', client: () => `glxinfo.${multiarch()} -B` },
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
    // A function: `archAsset` depends on `?arch=`, parsed after this module loads.
    closure: () => archAsset('/closure/dolphin'),
    closureImage: '/bin/sh',
    // Drop Mesa's software rasteriser from Dolphin's filesystem. Dolphin is a
    // QtWidgets app -- it renders through Qt's own raster engine into wl_shm and
    // needs no GL -- but on startup something (a KDE/EGL probe) dlopens
    // swrast_dri.so, whose mappings pushed the guest's 32-bit (4 GiB) wasm memory
    // over the top and crashed the tab. Absent, the probe fails gracefully and
    // Dolphin renders under the ceiling. Scoped to this app: glxgears' Xwayland
    // GLX genuinely needs swrast, and it does not drop it.
    dropFiles: [`/usr/lib/${multiarch()}/dri/swrast_dri.so`],
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
  // Every client in WL_APPS is a Debian aarch64 binary, so when `?gl=mesa`
  // routes one through gex-gl the glibc build of the marshaller is the one to
  // inject. Without this the musl pair went in and the client died at load
  // with "libc.so: cannot open shared object file" -- musl's soname, reported
  // against a glibc program, which reads like a missing library rather than
  // like the wrong ABI. SuperTuxKart borrows this demo's `enter` but declares
  // its own `directGlAbi`, so it was never affected and the gap stayed hidden.
  directGlAbi: 'glibc',
  title: 'Qt / GTK / KDE apps on gex-wayland',
  blurb:
    'A gex-native Wayland compositor lives in the shim; a client connects and its committed shm ' +
    'frames are blitted onto a canvas here. Pick a client from the dropdown: real Qt, GTK and KDE ' +
    'apps draw in software into wl_shm, or glxgears/es2gears render OpenGL.',
  network: false,

  async enter(ctx) {
    const demo = this;
    const wrap = document.createElement('div');
    wrap.className = 'canvas-wrap';
    // The compositor's output: the whole desktop, and the only thing the user
    // sees. Every surface is drawn here -- windows with the frames the
    // compositor draws, their menus, and the cursor -- by a WebGPU device in
    // the compositor worker. `tabIndex` makes it able to take keyboard focus,
    // which a bare <canvas> cannot.
    const wlCanvas = document.createElement('canvas');
    wlCanvas.id = 'gex-wl';
    wlCanvas.width = demo.outputWidth || 1024;
    wlCanvas.height = demo.outputHeight || 640;
    wlCanvas.tabIndex = 0;
    wlCanvas.classList.add('wl-output');
    wrap.append(wlCanvas);
    // The host GL device's own surface: the context an OpenGL client's commands
    // are replayed into. Not a window -- with a compositor the client's pixels
    // reach the screen the way every other client's do, through its
    // wl_surface -- so it is kept off-screen, rendering, and never shown.
    const canvas = document.createElement('canvas');
    canvas.width = demo.canvasWidth || 300;
    canvas.height = demo.canvasHeight || 300;
    canvas.classList.add('wl-offscreen');
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

    // There is one output now, so nothing is swapped: the FPS readout is the
    // only thing that depends on the client, since only a GL one has a frame
    // rate to report.
    const showWindow = (a) => {
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
      'the shim, and glxgears renders its OpenGL there too.',
    ];
    for (const line of intro) ctx.term.writeln(line);
    ctx.term.writeln('');

    const start = async (seconds, runOptions = {}) => {
      ctx.actions([]);
      running(true);
      const t0 = performance.now();
      let renderer = null;
      // Where the guest's GL runs. In the compositor's worker its frames are
      // textures in the context that draws the scene, so a GL window costs no
      // readback and no upload -- which is the whole reason Mesa is on the
      // host. `?glhost=page` keeps the old arrangement, where the device is
      // here and its last frame is blitted over the output.
      const glInCompositor = usesDirectGl(demo) &&
        new URLSearchParams(location.search).get('glhost') !== 'page';
      try {
        ctx.busy('loading the renderer');
        if (glInCompositor) {
          // The runtime starts it once the compositor's worker exists.
        } else if (usesDirectGl(demo)) {
          renderer = await createDirectGlDevice(canvas);
        }
        // Otherwise no host GL: the guest's own Mesa draws in software.
        if (renderer && !renderer.ok) {
          throw new Error(`the renderer would not start: ${renderer.error}`);
        }
        clearInterval(fpsTimer);
        cancelAnimationFrame(fpsRaf);
        fps.textContent = 'FPS: 0.0';
        let fpsFrames = 0;
        let fpsSerial = renderer?.presentedCount() ?? 0;
        let fpsAt = performance.now();
        globalThis.__gexDisplayFps = [];
        const countDisplayedFrame = () => {
          renderer?.display?.();
          const serial = renderer?.presentedCount() ?? 0;
          if (serial !== fpsSerial) {
            fpsFrames++;
            fpsSerial = serial;
          }
          fpsRaf = requestAnimationFrame(countDisplayedFrame);
        };
        fpsRaf = requestAnimationFrame(countDisplayedFrame);
        fpsTimer = setInterval(() => {
          const now = performance.now();
          const measured = fpsFrames * 1000 / (now - fpsAt);
          fps.textContent = `FPS: ${measured.toFixed(1)}`;
          globalThis.__gexDisplayFps.push(measured);
          fpsFrames = 0;
          fpsAt = now;
        }, 1000);
        // Live render state for a test harness: it can tell a client rendered
        // (lit>0) the moment it happens rather than waiting for the exit report.
        globalThis.__gexGlState = () => {
          try {
            return renderer?.glState() ?? '';
          } catch {
            return '';
          }
        };
        globalThis.__gexGlCommands = () => {
          try {
            return renderer?.commands() ?? null;
          } catch {
            return '';
          }
        };
        if (renderer) ctx.term.writeln(ctx.ansi.dim(`  ${renderer.describe().split('|').join('\r\n  ')}`));
        ctx.term.writeln('');

        const qp = new URLSearchParams(location.search);
        // Keep the stock graphics image as the default, but let heavier X11
        // applications bring their own traced/minimised rootfs through the
        // same Xwayland + Wayland pipeline. The URL is still fetched
        // from this site's rootfs directory; only the asset name changes.
        const rootfsUrl = archAsset(qp.get('wlrootfs') || demo.rootfs || '/rootfs/graphics.tar');
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
        if (usesDirectGl(demo)) {
          const directFiles = ['libGLESv2.so.2', 'libEGL.so.1', 'libGL.so.1'];
          const directPrefix = archAsset(demo.directGlAbi ? `/fs/${demo.directGlAbi}` : '/fs');
          await Promise.all(directFiles.map(async (name) => {
            // `no-store` for the same reason the gles-gears path uses it: these
            // are build artifacts under stable names, and the persistent CDP
            // browser will otherwise keep yesterday's guest driver after the
            // server has rebuilt it. A stale one here is not a subtle bug --
            // it cost this demo its GL context entirely, on both backends.
            const response = await fetch(`${directPrefix}/${name}?direct=1`,
                                         { cache: 'no-store' });
            if (!response.ok) throw new Error(`direct GL guest driver ${name}: ${response.status}`);
            files[`/usr/lib/gex-gl/${name}`] = new Uint8Array(await response.arrayBuffer());
            tree.modes[`/usr/lib/gex-gl/${name}`] = 0o755;
          }));
          // libGL.so.1 -- the GLX half, and the same object again -- goes in a
          // directory of its own, deliberately off the shared library path.
          //
          // GLX is how a traditional X program asks for a context, so without
          // this file that whole class cannot start. But Xwayland links libGL
          // too, for glamor, and it wants the GL 1.x entry points this library
          // has never had: put it where Xwayland looks and Xwayland dies at
          // load with `undefined symbol: glConvolutionParameterfv`, taking the
          // X server and every client with it. Only the client's own command
          // gets this directory on its path -- see `run` below.
          {
            const response = await fetch(`${directPrefix}/libGL.so.1`,
                                         { cache: 'no-store' });
            if (response.ok) {
              files['/usr/lib/gex-glx/libGL.so.1'] =
                new Uint8Array(await response.arrayBuffer());
              tree.modes['/usr/lib/gex-glx/libGL.so.1'] = 0o755;
            }
          }
        }
        // `?wlclient=` overrides the X client (default glxgears); `?wlenv=a=b;c=d`
        // adds env, so a probe needs no rebuild.
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
          `export LD_LIBRARY_PATH=${usesDirectGl(demo) ? '/usr/lib/gex-gl:' : ''}/usr/lib/${multiarch()}\n` +
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
        // SuperTuxKart presets) is the only extra override.
        //
        // An explicit `?wlclient=` / `?cmd=` beats the action's preset. It used
        // to lose, which silently discarded what the harness had asked to run:
        // `tests/stk-webgpu-fps.sh` passes a full `--race-now ...` command line
        // and was benchmarking STK's main menu instead, because the Run button
        // handed `stkLaunch` over the top of it.
        //
        // `?wlclient=` is two things at once: the id of a listed app, or a raw
        // command. Taking it literally here threw away the resolution
        // `pickInitialApp` had just done, so `?wlclient=es2gears` ran a
        // program called `es2gears` -- "No such file or directory" -- instead
        // of /usr/bin/es2gears_wayland.aarch64-linux-gnu. The app's `mode`
        // came from the resolved entry while its command did not, which is
        // the shape of the bug: half-resolved.
        const wanted = qp.get('wlclient');
        const explicitClient =
          (wanted && (resolveClient(WL_APPS.find((a) => a.id === wanted)?.client) ?? wanted)) ||
          qp.get('cmd');
        const client = explicitClient || runOptions.client || resolveClient(app.client);
        // The GLX library reaches the client and nothing else; see where it
        // is staged above.
        const glxPath = usesDirectGl(demo)
          ? 'LD_LIBRARY_PATH=/usr/lib/gex-glx:$LD_LIBRARY_PATH ' : '';
        const run =
          `echo "gex: running ${client}"\n` +
          `${glxPath}${runClient(client)} 2>&1\n` +
          'gex_client_status=$?\n' +
          (runOptions.after ? `${runOptions.after}\n` : '') +
          'echo "gex: client exited $gex_client_status"\n';
        let script;
        if (app.mode === 'shm') {
          // A native Wayland toolkit client (GTK's wayland backend, Qt's
          // libqwayland-generic) drawing in software into wl_shm -- no X, no GL.
          // The app's own env (QT_QPA_PLATFORM=wayland, GDK_BACKEND=wayland, the
          // plugin paths) goes on top of the base env.
          script = baseEnv + (app.env || '') + run;
        } else if (app.mode === 'gl') {
          // A wayland-native GL client straight against gex-wayland -- no X at
          // all. EGL's wl_shm/drisw path needs no gbm/DRM.
          // EGL_PLATFORM=wayland is what makes Mesa's EGL take the wl_shm/drisw
          // path instead of probing for a GPU/gbm device
          // it cannot find. Without it eglInitialize fails; with it, it comes up
          // and enumerates real configs.
          script = baseEnv + 'export EGL_PLATFORM=wayland\n' + (app.env || '') + run;
        } else {
          // An X client. The X server is the one in this page (a Worker
          // holding `Xwayland.wasm`; see web/src/hostx.js), so this script
          // starts nothing: it exports the display and runs the client.
          //
          // There used to be a guest Xwayland here -- `Xwayland :0 -ac &` and
          // six seconds to let it bind. It is gone everywhere: it translated a
          // whole X server and Mesa into a tab, and with the guest's Mesa
          // retired it could not even load, because it links libGL for glamor
          // and the only libGL in the image is a marshaller that does not
          // pretend to have every legacy entry point (`undefined symbol:
          // glConvolutionParameterfv`). What kept working was the page's
          // server answering :0 underneath the corpse.
          script =
            baseEnv +
            (app.env || '') +
            'echo "gex: the X server is in the page"\n' +
            'export DISPLAY=:0\n' +
            // No `gdk-pixbuf-query-loaders` here: it hangs this pipeline (a
            // forked child that never returns, so the shell's `wait` blocks
            // and the client never starts), and it could not help anyway --
            // it writes its cache into THIS shell's per-process tmpfs, which
            // a forked client does not share. A GTK demo that needs the
            // loader cache must build it a way that crosses the process
            // boundary, or bake it into the rootfs.
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
        const closurePath = qpc.get('appclosure') || (typeof app.closure === 'function' ? app.closure() : app.closure);
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
          // Host GL on the direct path: the guest only marshals and the
          // host's Mesa does the work. The output reaches the screen through
          // the client's wl_surface like everything else.
          gl: glInCompositor ? 'compositor' : (usesDirectGl(demo) ? renderer : null),
          wl: wlCanvas,
          wlInput: wlCanvas,
          // The X server for the `x` apps above: in the page, started before
          // the guest so the display is already listening. `?hostx=0` is left
          // as a way to run without one at all, which is what a Wayland-only
          // app wants and all a `shm`/`gl` app ever needed.
          hostX: new URLSearchParams(location.search).get('hostx') !== '0',
          heartbeat: Number(new URLSearchParams(location.search).get('heartbeat')) || 0,
        });
        const ms = (performance.now() - t0).toFixed(0);
        ctx.term.writeln('');
        rule(ctx, `exit ${status} in ${ms} ms`);
        await renderer?.refresh?.();
        const gl = renderer?.glState() ?? '';
        const nums = ['presented', 'lit', 'brightest']
          .map((k) => `${k}=${/=(\d+)/.exec(gl.slice(gl.indexOf(k)))?.[1] ?? '?'}`)
          .join(' ');
        ctx.term.writeln(ctx.ansi.dim(`  ${nums}`));
        ctx.term.writeln(ctx.ansi.dim(`  ${gl}`));
        // Null on the Mesa backend, which keeps no command counts.
        for (const part of (renderer?.commands() ?? '').split('|')) {
          if (part.trim()) ctx.term.writeln(ctx.ansi.dim(`  ${part.trim()}`));
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
  '--screensize=640x480 --windowed --disable-hd-textures ' +
  '--disable-glow --disable-bloom --disable-light-shaft --disable-dof --disable-particles ' +
  '--disable-animated-characters --disable-motion-blur --disable-mlaa --disable-ssao ' +
  '--disable-ibl --shadows=0 --anisotropic=0';
const stkRace =
  `/usr/games/supertuxkart --race-now --track=snowtuxpeak --kart=tux ` +
  `--numkarts=1 --laps=1 ${stkVideoOptions}`;
const stkPlay = `/usr/games/supertuxkart ${stkVideoOptions}`;
// The same environment the SuperTuxKart card sets, as shell exports, for a
// session that runs it beside other programs.
const stkEnv =
  'export IRR_DEVICE_TYPE=x11\n'
  + 'export SUPERTUXKART_DATADIR=/usr/share/games/supertuxkart\n'
  + 'export ALSOFT_DRIVERS=null\n'
  + 'export MESA_GL_VERSION_OVERRIDE=3.3\n'
  + 'export MESA_GLSL_VERSION_OVERRIDE=330\n'
  + 'export JSIMD_FORCENONE=1\n';
const stkProfileValue = Number(new URLSearchParams(location.search).get('stkprofile'));
// Preserve the original `?stkprofile=1` harness shorthand (a five-second
// profile), while accepting an explicit duration for longer stable samples.
// Which backend a directGl demo renders through: Mesa (st/mesa) below gex's
// wire decoder, on gex_wgpu (see mesaBackend).
/* Whether this demo sends its GL straight to the host's Mesa, rather than
 * drawing in software with the guest's own Mesa.
 *
 * `?gl=` forces it on for a demo that does not declare it, which is how a
 * second application can be put on the Mesa backend at all -- SuperTuxKart is
 * the only demo that declares `directGl`.
 *
 * It has to be one decision rather than a per-call-site test: the flag also
 * chooses the guest's LD_LIBRARY_PATH.
 *
 * Forcing it only makes sense for a client that reaches GL through **EGL**.
 * gex-gl ships `libEGL.so.1` and `libGLESv2.so.2` and no `libGL.so.1`, so
 * there is no GLX provider on this path: an ordinary GLX client (glxgears)
 * cannot use it, and Xwayland -- which finds gex-gl first on
 * LD_LIBRARY_PATH and then cannot resolve what libepoxy wants -- aborts with
 * SIGABRT inside libepoxy rather than falling back. SuperTuxKart is
 * unaffected because Irrlicht asks EGL for desktop GL directly, which is why
 * it negotiates GL 3.1 here without any GLX at all. Giving GLX clients this
 * backend needs a libGL that gex-gl does not currently build. */
function usesDirectGl(demo) {
  const forced = new URLSearchParams(location.search).get('gl');
  return !!(demo.directGl || forced);
}

async function createDirectGlDevice(canvas) {
  const query = new URLSearchParams(location.search);
  const { MesaGlDevice } = await import('./mesa-gl.js');
  // Desktop core by default here: a real GL application asks for a core
  // profile, unlike the ES demos that share this path.
  return MesaGlDevice.create(canvas, mesaBackend(query, 'core'));
}

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
    'Run the stock AArch64 SuperTuxKart package through gex-wayland and Xwayland, with ' +
    'its OpenGL calls run by st/mesa in this browser, on the gex_wgpu Gallium driver.',
  network: false,
  // Match STK's framebuffer, Xwayland's advertised output, and the displayed
  // canvas one-for-one instead of rendering tiny and stretching in CSS.
  canvasWidth: 640,
  canvasHeight: 480,
  directGl: true,
  // Xwayland and Debian's stock STK use glibc. The small GLES demo is musl;
  // keeping both driver builds avoids injecting a second C runtime into either.
  directGlAbi: 'glibc',
  // Version the generated image so a persistent browser does not reuse an
  // earlier over-trimmed tar that lacked the track and kart materials.
  rootfs: archAsset('/rootfs/stk.tar?v=snowpeak20'),
  env:
    'IRR_DEVICE_TYPE=x11;SUPERTUXKART_DATADIR=/usr/share/games/supertuxkart;' +
    'ALSOFT_DRIVERS=null;JSIMD_FORCENONE=1',
  // Auto-run is playable by default. The harness adds ?stkprofile=1 for its
  // finite benchmark run without exposing profiling controls in the demo UI.
  client: stkLaunch,
  intro: [
    'SuperTuxKart is running as an AArch64 X11 application inside this page:',
    'Xwayland -> gex-wayland; OpenGL -> gex-gl -> Mesa in this page -> WebGPU.',
  ],
  hint: 'SuperTuxKart and Xwayland rendering straight to WebGPU',
  actions: (start) => {
    const run = () => start(300, {
      client: stkLaunch,
      noTimeout: true,
      // Hidden profile runs preserve the application's own logfile after an
      // abnormal exit; normal user-facing runs do not add harness output.
      after: stkProfileSeconds > 0
        ? 'echo "gex: STK logfile tail"; tail -n 120 /root/.config/supertuxkart/config-0.10/stdout.log 2>/dev/null || true'
        : '',
    });
    return [
      { label: 'Run on Mesa', primary: true, onClick: () => run() },
    ];
  },
  enter: waylandGears.enter,
};


// -- the compositor demo ----------------------------------------------------
//
// Not one app on a canvas: a session. Several programs, each its own guest
// process, all connected to the compositor in the shim, all drawn into one
// output with frames the compositor draws itself -- and, for the X11 ones, a
// rootless Xwayland that gives every X window its own wl_surface so the
// compositor decorates and stacks them like any other window.
const SESSION_APPS = [
  {
    id: 'kcalc',
    label: 'KCalc — Qt 5 / KF5, native Wayland',
    kind: 'wl',
    cmd: 'kcalc',
    env: WL_TOOLKIT_ENV.qt5,
  },
  {
    id: 'qt6ct',
    label: 'Qt 6 Configuration — native Wayland',
    kind: 'wl',
    cmd: 'qt6ct',
    env: WL_TOOLKIT_ENV.qt6,
  },
  {
    id: 'explosive-c4',
    label: 'Explosive C4 — Qt 6 game, native Wayland',
    kind: 'wl',
    cmd: 'explosive-c4',
    env: WL_TOOLKIT_ENV.qt6,
  },
  {
    id: 'gtk3-demo',
    label: 'GTK 3 Demo — native Wayland (draws its own frames)',
    kind: 'wl',
    cmd: 'gtk3-demo',
    env: WL_TOOLKIT_ENV.gtk,
  },
  {
    id: 'dolphin',
    label: 'Dolphin — KDE file manager, native Wayland',
    kind: 'wl',
    cmd: 'dolphin --new-window /root',
    env: WL_TOOLKIT_ENV.qt5 + 'export XDG_CONFIG_HOME=/root/.config\n',
  },
  { id: 'xeyes', label: 'xeyes — X11, on the X server in this page', kind: 'x', cmd: 'xeyes' },
  { id: 'xclock', label: 'xclock — X11, on the X server in this page', kind: 'x', cmd: 'xclock' },
  {
    id: 'xterm',
    label: 'xterm — a terminal, on the X server in this page',
    kind: 'x',
    // With a script rather than bare: xterm with no `-e` looks for a shell in
    // the password file, and this image has no entry to find.
    cmd: 'xterm -e /bin/sh /usr/bin/shell.sh',
    // xterm wants no GL of its own, but what a person types into it might:
    // the marshaller goes on this subshell's library path, and everything
    // launched from the terminal inherits it. Without that, a glxgears typed
    // at the prompt finds no libGL at all -- the image carries no Mesa.
    glEnv: true,
    on: true,
  },
  { id: 'xcalc', label: 'xcalc — X11, on the X server in this page', kind: 'x', cmd: 'xcalc' },
  {
    id: 'glxgears',
    // OpenGL, so it needs the marshaller on its path: see `directGl`.
    gl: true,
    label: 'glxgears — OpenGL, marshalled to Mesa on the host',
    kind: 'x',
    cmd: () => `glxgears.${multiarch()}`,
    on: true,
  },
  {
    /* An SDL-shaped GLX initialization, without SDL: a throwaway window and
     * context to read the GL strings, both destroyed, then the real ones with
     * an explicit version and profile -- and a redundant make-current in
     * every frame. That sequence is what toolkits do and what small demos
     * never do, and it is what made SDL applications need a preloaded
     * interposer here for years. See guest/glx-init/glx-init.c. */
    id: 'glx-init',
    label: 'glx-init — a toolkit-shaped GLX startup (prints each step)',
    kind: 'x',
    cmd: '/usr/bin/glx-init 120',
    gl: true,
  },
  {
    /* The same question on the other path: a desktop-GL context through EGL,
     * which is how every Wayland client and SDL's EGL backend reach the
     * device. No window, because what is being asked about is the context and
     * what it says about itself -- see guest/glx-init/egl-init.c. */
    id: 'egl-init',
    label: 'egl-init — desktop GL through EGL (prints what the context says)',
    kind: 'wl',
    cmd: '/usr/bin/egl-init 120',
    gl: true,
  },
  {
    // The smallest GL client there is, and the one that finds driver bugs the
    // big ones hide: it points glVertexAttribPointer at a stack array with no
    // buffer bound, the way tutorial code and every mesa-demos program does.
    //
    // It currently draws upside down, and it is the only one that does: its
    // binary's vertex data is red at (-1,-1), green at (1,-1) and blue at
    // (0,1) -- blue at the top -- and what appears has blue at the bottom.
    // glxgears through GLX, es2gears on Wayland and es2gears on X11 are all
    // the right way up, in both compositors, so this is something about this
    // client's own path through the marshaller rather than the orientation of
    // a presented frame. Left in the picker because it is the smallest case
    // that shows it.
    id: 'es2tri',
    label: 'es2tri — a triangle, client-side arrays (renders flipped)',
    kind: 'x',
    cmd: () => `/usr/bin/es2tri.${multiarch()}`,
    gl: true,
  },
  {
    id: 'glmark2',
    // OpenGL, so it needs the marshaller on its path: see `directGl`.
    gl: true,
    label: 'glmark2 — a real GL benchmark, on the host\'s Mesa',
    kind: 'x',
    cmd: 'glmark2 --size 480x320',
  },
  {
    id: 'es2gears',
    gl: true,
    label: 'es2gears — OpenGL ES straight onto Wayland',
    kind: 'wl',
    cmd: () => `/usr/bin/es2gears_wayland.${multiarch()}`,
    env: 'export EGL_PLATFORM=wayland\n',
  },
  {
    id: 'supertuxkart',
    gl: true,
    label: 'SuperTuxKart — a real game, X11 + Mesa (needs the big image)',
    kind: 'x',
    cmd: stkRace,
    env: stkEnv,
    // Its 200 MB of game data is in its own image, so choosing it swaps the
    // session's filesystem for the one that has both.
    rootfs: '/rootfs/session-stk.tar',
  },
];

/** One labelled row of radios, for a choice the session makes on its next run.
 *
 * The two of these -- which compositor, which Mesa -- are the same shape, and
 * both are things a person should be able to try without editing a URL. */
function radioGroup({ legend, name, current, options, onpick }) {
  const group = document.createElement('div');
  group.className = 'session-choice';
  group.append(Object.assign(document.createElement('span'), { textContent: legend }));
  for (const [value, text] of options) {
    const label = document.createElement('label');
    const radio = document.createElement('input');
    radio.type = 'radio';
    radio.name = name;
    radio.value = value;
    radio.checked = current === value;
    radio.onchange = () => {
      if (!radio.checked || current === value) return;
      current = value;
      onpick(value);
    };
    label.append(radio, document.createTextNode(` ${text}`));
    group.append(label);
  }
  return group;
}

/**
 * The session script: the environment every client needs, then each chosen app
 * in the background.
 *
 * The X clients share one Xwayland, started `-rootless` so that each X window
 * becomes its own `xdg_toplevel` rather than one big desktop -- which is what
 * makes an X11 program and a Wayland program windows of the same session.
 * Launches are spaced out because each new process translates its own copy of
 * whatever it links, and six toolkits arriving at once is a memory spike with
 * nothing to show for it.
 */
function sessionScript(chosen, {
  stagger = 4, verbose = 0, extra = '', directGl = true,
} = {}) {
  const wl = chosen.filter((a) => a.kind === 'wl');
  const x = chosen.filter((a) => a.kind === 'x');
  // An X server is wanted if anything needs one, including `?xtra=`.
  const wantX = x.length > 0 || !!extra;
  let s = '';
  s += 'export XDG_RUNTIME_DIR=/tmp\n';
  s += 'export WAYLAND_DISPLAY=wayland-0\n';
  s += `export LD_LIBRARY_PATH=/usr/lib/${multiarch()}\n`;
  s += 'export HOME=/root\n';
  s += 'export XDG_CACHE_HOME=/tmp\n';
  s += 'export XDG_DATA_DIRS=/usr/share\n';
  s += 'export GSETTINGS_SCHEMA_DIR=/usr/share/glib-2.0/schemas\n';
  s += 'export SHELL=/bin/sh\n';
  s += 'export TERM=xterm\n';
  s += 'export FONTCONFIG_PATH=/etc/fonts\n';
  s += 'export DBUS_SESSION_BUS_ADDRESS=disabled:\n';
  s += 'export QT_LOGGING_RULES="*=false"\n';
  s += 'export TCL_LIBRARY=/usr/share/tcltk/tcl8.6\n';
  s += 'export TK_LIBRARY=/usr/share/tcltk/tk8.6\n';
  // No shader cache: the cache runs a second thread whose handshake with the
  // first is a cross-Worker futex the browser scheduler can drop.
  s += 'export MESA_SHADER_CACHE_DISABLE=true\n';
  s += 'export MESA_GLSL_CACHE_DISABLE=1\n';
  s += 'echo "gex: session starting"\n';
  for (const a of wl) {
    // Each app's own environment in a subshell, so one app's QT_QPA_PLATFORM
    // is not every app's.
    const path = directGl && (a.gl || a.glEnv)
      ? 'LD_LIBRARY_PATH=/usr/lib/gex-gl:$LD_LIBRARY_PATH '
      : '';
    s += `(\n${a.env || ''}echo "gex: launching ${a.id}"\n${path}${resolveClient(a.cmd)} 2>&1\n) &\n`;
    s += `sleep ${stagger}\n`;
  }
  if (wantX) {
    // Rootless: one X server, and every X window is a wl_surface of its own.
    //
    // No window manager is started here, and that is the point: rootless
    // Xwayland hands a window to Wayland only once it is *managed*, and the
    // thing that manages it is the compositor. gex-wayland contains the X
    // window manager (`shims/browser/src/xwm.rs`), exactly as sway, mutter and
    // kwin do -- it takes SubstructureRedirect on the root, answers
    // MapRequest, and pairs each X window with the surface Xwayland names in
    // its WL_SURFACE_ID message.
    // The X server is a wasm module in the page (see web/src/hostx.js),
    // started before this shell and already listening. Its socket is a name in
    // the global kernel, which is where a guest Xwayland would have bound one,
    // so from a client's side nothing about this is different -- and there is
    // no guest Xwayland to start any more: the image does not carry one.
    s += 'echo "gex: the X server is in the page"\n';
    s += 'export DISPLAY=:0\n';

    for (const a of x) {
      s += `echo "gex: launching ${a.id}"\n`;
      // An OpenGL client gets the marshaller on its path, and so does a
      // terminal, whose children may want it (see `glEnv`). Not everything,
      // still: this list is what asks for GL, and a program that never does
      // should not have its libGL answered by a marshaller. The old reason --
      // a guest Xwayland linking libGL for glamor and dying at load on it --
      // is gone with the guest Xwayland.
      const path = directGl && (a.gl || a.glEnv)
        ? 'LD_LIBRARY_PATH=/usr/lib/gex-glx:/usr/lib/gex-gl:$LD_LIBRARY_PATH '
        : '';
      s += `(\n${a.env || ''}${path}${resolveClient(a.cmd)} 2>&1\n) &\n`;
      s += `sleep ${stagger}\n`;
    }
    // `?xtra=<command>` runs one more X client, whatever it is: a way to try
    // something the picker does not list without rebuilding anything.
    if (extra) {
      s += `echo "gex: launching ${extra}"\n`;
      // With the GL path, since what this is for is trying a client the
      // picker does not list -- and half of those are GL clients.
      const path = directGl ? 'LD_LIBRARY_PATH=/usr/lib/gex-glx:/usr/lib/gex-gl:$LD_LIBRARY_PATH ' : '';
      s += `${path}${extra} 2>&1 &\n`;
      s += `sleep ${stagger}\n`;
    }
  }
  s += 'echo "gex: session up"\n';
  s += 'wait\n';
  return s;
}

/* Window decorations, in HTML, for the DOM desktop.
 *
 * This is the part a page owns: the compositor hands over a canvas per window
 * and nothing else, and everything here -- the frame, the title, dragging,
 * closing, stacking -- is ordinary DOM. It is deliberately plain, because the
 * point is that it is replaceable: a page can style it, animate it, or lay the
 * windows out in a grid instead, without the compositor knowing.
 */
function domDecorations(root, ctx) {
  root.style.position = 'relative';
  root.style.overflow = 'hidden';
  let top = 10;
  let cascade = 0;
  // The desktop is created by the runtime when the run starts; poll for it
  // rather than plumb a callback through, since a page would normally be
  // holding the object it made itself. The caller is handed a way to stop
  // waiting, because choosing the other compositor while this one is still
  // looking would leave two of these running.
  const wait = setInterval(() => {
    const desk = globalThis.__gexDesktop;
    if (!desk || desk.onwindow) return;
    clearInterval(wait);
    desk.onwindow = (win) => {
      const frame = document.createElement('div');
      frame.className = 'dom-window';
      const bar = document.createElement('div');
      bar.className = 'dom-titlebar';
      const name = document.createElement('span');
      name.textContent = win.title || 'X11';
      const close = document.createElement('button');
      close.textContent = '\u2715';
      close.className = 'dom-close';
      bar.append(name, close);
      if (win.decorations === 'server') frame.append(bar);
      frame.append(win.canvas);
      // A cascade, so two windows that appear together are both reachable.
      const x = 12 + (cascade * 28) % Math.max(1, root.clientWidth - win.width - 24);
      const y = 12 + (cascade * 28) % Math.max(1, root.clientHeight - win.height - 60);
      cascade++;
      const put = (nx, ny) => {
        frame.style.left = `${nx}px`;
        frame.style.top = `${ny}px`;
        // The compositor is told where the window is because an X client's
        // menus are placed in these coordinates. A Wayland one never asks.
        win.place(nx + (win.decorations === 'server' ? 0 : 0),
                  ny + (win.decorations === 'server' ? bar.offsetHeight : 0));
      };
      frame.style.position = 'absolute';
      frame.style.zIndex = String(++top);
      root.append(frame);
      put(x, y);

      win.onretitle = (t) => { name.textContent = t; };
      win.onresized = () => put(parseInt(frame.style.left, 10), parseInt(frame.style.top, 10));
      win.onclosed = () => frame.remove();
      close.onclick = () => win.close();
      frame.addEventListener('pointerdown', () => {
        frame.style.zIndex = String(++top);
        win.focus();
      }, true);

      // Dragging by the titlebar, which is a page's business and not a
      // compositor's: nothing below knows this happened except through
      // `place`.
      let from = null;
      bar.addEventListener('pointerdown', (e) => {
        from = { x: e.clientX, y: e.clientY,
                 left: parseInt(frame.style.left, 10), top: parseInt(frame.style.top, 10) };
        // A pointer the browser no longer considers active -- a click that
        // ended before this handler ran, an element already on its way out of
        // the page -- makes this throw rather than return false. Dragging is
        // not worth an exception.
        try { bar.setPointerCapture(e.pointerId); } catch { /* drag without it */ }
      });
      bar.addEventListener('pointermove', (e) => {
        if (!from) return;
        put(from.left + (e.clientX - from.x), from.top + (e.clientY - from.y));
      });
      bar.addEventListener('pointerup', (e) => {
        from = null;
        try { bar.releasePointerCapture(e.pointerId); } catch { /* never held */ }
      });

      // A popup or an X menu is placed by its client, in output coordinates.
      win.onplace = (px, py) => {
        frame.style.left = `${px}px`;
        frame.style.top = `${py}px`;
      };
      ctx?.hint?.(`${desk.windows().length} window(s)`);
    };
  }, 100);
  return () => clearInterval(wait);
}

const compositor = {
  id: 'compositor',
  title: 'A desktop session on gex-wayland',
  blurb:
    'gex-wayland as a compositor: several AArch64 programs at once, each its own guest process, '
    + 'in windows this page draws the frames for. Qt and KDE apps take server-side decorations, '
    + 'GTK draws its own, X11 programs arrive through an X server that is part of this page '
    + 'rather than a guest program, and OpenGL is marshalled out of the guest to Mesa in the '
    + 'compositor\'s own worker -- so a GL window is a texture it draws rather than a frame '
    + 'copied twice. Nothing in the guest is a graphics driver. Every window is drawn from the '
    + 'guest\'s own memory: nothing copies a frame. The X11 clients share one X server and are '
    + 'cheap; a Qt or KDE toolkit translates 100 MB of its own code, so pick one of those at a '
    + 'time. Two choices below: which compositor draws the windows -- one WebGPU scene, or an element '
    + 'each with the frames in this page\'s HTML. Mesa runs on gex_wgpu, on WebGPU.',
  // The image has apt in it, so a session can install into itself -- which
  // needs a Wisp relay, set with `?wisp=` (see the Debian demo).
  network: true,
  outputWidth: 1280,
  outputHeight: 760,
  // The session image: every program the picker offers, and Mesa's software
  // rasteriser stubbed out -- nothing here enters it, and a browser tab pays
  // for every megabyte of an image twice (once fetched, once written into the
  // guest's filesystem).
  rootfs: '/rootfs/session.tar',

  async enter(ctx) {
    const wrap = document.createElement('div');
    wrap.className = 'canvas-wrap';
    // Which compositor draws this session. Both are the same compositor
    // underneath -- the same surfaces, the same input, the same protocol in
    // every guest -- and differ in where the pixels land: one WebGPU scene that
    // draws its own decorations, or an element per window with the frames in
    // this page's HTML. Chosen in the picker below, or with `?desktop=`.
    //
    // The output element itself differs (a canvas or a div), so it is built
    // per choice rather than switched: `output()` replaces it in place, which
    // is only allowed between runs.
    let mode = new URLSearchParams(location.search).get('desktop') === 'dom' ? 'dom' : 'scene';
    const glMode = 'wgpu';
    let wlCanvas = null;
    const buildOutput = () => {
      const asDom = mode === 'dom';
      const el = document.createElement(asDom ? 'div' : 'canvas');
      el.id = 'gex-wl';
      if (asDom) {
        el.style.width = `${this.outputWidth}px`;
        el.style.height = `${this.outputHeight}px`;
      } else {
        el.width = this.outputWidth;
        el.height = this.outputHeight;
      }
      el.tabIndex = 0;
      el.classList.add(asDom ? 'dom-output' : 'wl-output');
      if (wlCanvas) wlCanvas.replaceWith(el);
      else wrap.append(el);
      wlCanvas = el;
      stopDecorations?.();
      stopDecorations = asDom ? domDecorations(el, ctx) : null;
      return el;
    };
    let stopDecorations = null;
    buildOutput();
    ctx.onLeave(() => stopDecorations?.());

    // Which apps to start. A query string wins (`?apps=kcalc,xterm`), so a
    // harness needs no clicking.
    const qp = new URLSearchParams(location.search);
    const wanted = qp.get('apps')?.split(',').filter(Boolean);
    const picker = document.createElement('div');
    picker.className = 'wl-picker session-picker';
    const boxes = new Map();
    for (const a of SESSION_APPS) {
      const label = document.createElement('label');
      const box = document.createElement('input');
      box.type = 'checkbox';
      box.checked = wanted ? wanted.includes(a.id) : !!a.on;
      label.append(box, document.createTextNode(` ${a.label}`));
      picker.append(label);
      boxes.set(a.id, box);
    }
    // And which compositor draws them, and which Mesa draws the OpenGL.
    // Radios rather than checkboxes because each is one choice, and both take
    // effect on the next run: the desktop and the GL device are built when the
    // session starts.
    picker.append(radioGroup({
      legend: 'windows drawn by:',
      name: 'gex-desktop',
      current: mode,
      options: [
        ['scene', 'one WebGPU scene (decorations drawn by the compositor)'],
        ['dom', 'an element each (decorations in this page\'s HTML)'],
      ],
      onpick: (value) => {
        mode = value;
        buildOutput();
        ctx.hint(value === 'dom'
          ? 'windows will be elements, with the frames in HTML'
          : 'windows will be drawn in one WebGPU scene');
      },
    }));
    document.getElementById('terminal')?.before(wrap);
    wrap.before(picker);
    ctx.onLeave(() => {
      picker.remove();
      wrap.remove();
    });

    for (const line of [
      'A session, not one window: every program below is its own AArch64',
      'process, and what draws them is gex-wayland in the shim -- which is',
      'also the X window manager the rootless Xwayland needs.',
      '',
      'Drag a titlebar, click between windows, type into the one with the',
      'focus. Each toolkit app costs a tab about 3 GB while it translates,',
      'so a session of several is X11 clients plus one of them.',
    ]) {
      ctx.term.writeln(line);
    }
    ctx.term.writeln('');

    const start = async (seconds) => {
      ctx.actions([]);
      running(true);
      // A fresh output for every run. The scene compositor takes the canvas
      // over -- `transferControlToOffscreen` -- and a canvas allows that once,
      // so re-running the session on the one from last time throws before
      // anything starts. Rebuilding is also what clears the previous run's
      // windows out of the DOM desktop.
      /* XXX test: no rebuild */
      const chosen = SESSION_APPS.filter((a) => boxes.get(a.id)?.checked);
      if (!chosen.length && !qp.get('xtra')) {
        ctx.term.writeln('nothing selected');
        running(false);
        return;
      }
      const t0 = performance.now();
      // OpenGL runs on the host: the guest marshals, Mesa is in the
      // compositor's worker, and a frame is a texture that worker draws.
      try {
        ctx.busy('unpacking the rootfs');
        // The smallest image that has everything chosen: the graphics one
        // unless something needs the session image, which is that plus
        // SuperTuxKart's 200 MB of game data.
        const image = chosen.find((a) => a.rootfs)?.rootfs || this.rootfs;
        const tree = await ctx.rootfs(archAsset(qp.get('wlrootfs') || image));
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
        // The X server is the one in the page (`web/src/hostx.js`): native wasm
        // rather than a translated guest program, and the only one there is --
        // the image no longer carries an Xwayland to run.
        //
        // Its lack of GLX used to be the reason a session with an OpenGL
        // client started a guest server instead. It is not any more: a GL
        // client's OpenGL does not go through X at all now. It marshals to
        // Mesa in the compositor's worker and the frame arrives as a texture.
        const hostX = qp.get('hostx') !== '0';
        const script = sessionScript(chosen, {
          directGl: true,
          stagger: Number(qp.get('stagger')) || 4,
          // `?xverbose=N` turns the X server's own log on, which is the only
          // thing that says where a session that never appears stopped.
          verbose: Number(qp.get('xverbose')) || 0,
          extra: qp.get('xtra') || '',
        });
        // The guest's GL driver, when the session renders on the host: three
        // sonames of one marshaller, in a directory of their own. `gex-glx`
        // is separate because Xwayland links libGL for glamor and dies at
        // load if it finds this one -- only the client's own command gets it.
        {
          for (const [from, to] of [
            ['/fs/glibc/libGLESv2.so.2', '/usr/lib/gex-gl/libGLESv2.so.2'],
            ['/fs/glibc/libEGL.so.1', '/usr/lib/gex-gl/libEGL.so.1'],
            ['/fs/glibc/libGL.so.1', '/usr/lib/gex-glx/libGL.so.1'],
            // A guest program rather than a driver, staged the same way: see
            // the `glx-init` entry in the picker.
            ['/fs/glibc/glx-init', '/usr/bin/glx-init'],
            ['/fs/glibc/egl-init', '/usr/bin/egl-init'],
          ]) {
            const response = await fetch(archAsset(from), { cache: 'no-store' });
            if (!response.ok) continue;
            files[to] = new Uint8Array(await response.arrayBuffer());
            tree.modes[to] = 0o755;
          }
        }
        files['/session.sh'] = new TextEncoder().encode(script);
        ctx.idle();
        rule(ctx, `graphics rootfs — ${Object.keys(files).length} files, ${chosen.length} apps`);
        ctx.status('running');
        const { status } = await ctx.runProgram(ctx.term, {
          elf: files['/bin/sh'],
          files,
          dirs: tree.dirs,
          modes: tree.modes,
          args: ['/bin/sh', '/session.sh'],
          cwd: '/',
          tty: true,
          uid: 0,
          // The guest marshals to Mesa in the compositor's worker.
          gl: 'compositor',
          glBackend: glMode,
          wl: wlCanvas,
          wlInput: wlCanvas,
          hostX,
          heartbeat: Number(qp.get('heartbeat')) || 0,
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
        ctx.actions(actions(start));
      }
    };

    const actions = (go) => [
      { label: 'Start the session', primary: true, onClick: () => go(0) },
    ];
    ctx.actions(actions(start));
    ctx.hint('drag a titlebar, click a window, type into it');
  },
};

export const DEMOS = [
  upload,
  busybox,
  debian,
  gears,
  waylandGears,
  compositor,
  supertuxkart,
];
