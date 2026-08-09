// The demos.
//
// Each is an object with an `enter(ctx)` that owns the terminal until the
// next one is chosen. `ctx` carries the page's chrome -- the terminal, the
// overlay, the status line -- so a demo never touches the DOM itself.

const running = (on) => window.dispatchEvent(new CustomEvent('gex:running', { detail: on }));

/** Print a rule with a label, so a long session stays readable. */
function rule(ctx, label) {
  const w = Math.max(20, ctx.term.cols - label.length - 4);
  ctx.term.writeln(ctx.ansi.dim(`── ${label} ${'─'.repeat(w)}`));
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
        ctx.term.writeln(ctx.ansi.red(`\r\n${e.message}`));
        ctx.status(e.message, 'bad');
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
    ctx.actions([{ label: 'Boot', primary: true, id: 'boot', onClick: () => boot(ctx) }]);
    ctx.hint('a real shell: try `ls -l /bin`, `ps`, `echo hi | wc -c`');
    ctx.term.writeln('A BusyBox rootfs, about 1 MB, unpacked into an in-memory filesystem.');
    ctx.term.writeln('');
    ctx.term.writeln(ctx.ansi.dim('Press Boot. The first command is slow -- the whole binary has'));
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
    // The applet links are the whole of what makes a BusyBox rootfs a
    // userspace: one binary answering to eighty names.
    for (const [path, target] of Object.entries(tree.links)) {
      const abs = target.startsWith('/')
        ? target
        : `${path.slice(0, path.lastIndexOf('/'))}/${target}`.replace(/\/{2,}/g, '/');
      if (files[abs]) files[path] = files[abs];
    }
    const elf = files['/bin/busybox'];
    if (!elf) throw new Error('the rootfs has no /bin/busybox');
    ctx.idle();
    ctx.term.reset();
    ctx.term.writeln(ctx.ansi.dim(`rootfs: ${Object.keys(files).length} files`));
    ctx.term.writeln('');
    ctx.status('running');
    running(true);
    const { status } = await ctx.runProgram(ctx.term, {
      elf,
      files,
      args: ['/bin/sh', '-i'],
      cwd: '/root',
      tty: true,
    });
    ctx.term.writeln(ctx.ansi.dim(`\r\n[shell exited ${status}]`));
    ctx.status('finished', 'good');
  } catch (e) {
    ctx.term.writeln(ctx.ansi.red(`\r\n${e.message}`));
    ctx.status(e.message, 'bad');
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
      { label: 'Boot', primary: true, id: 'dboot', onClick: () => bootDebian(ctx) },
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
const wispUrl = () => localStorage.getItem(WISP_KEY) || '';

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
  try {
    const tree = await ctx.rootfs('/rootfs/debian.tar');
    const files = { ...tree.files };
    for (const [path, target] of Object.entries(tree.links)) {
      const abs = target.startsWith('/')
        ? target
        : `${path.slice(0, path.lastIndexOf('/'))}/${target}`.replace(/\/{2,}/g, '/');
      if (files[abs]) files[path] = files[abs];
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

    ctx.idle();
    ctx.term.reset();
    ctx.term.writeln(ctx.ansi.dim(`rootfs: ${Object.keys(files).length} files`));
    ctx.term.writeln(
      ctx.ansi.dim(network ? 'network: wisp' : 'network: none -- apt will not reach a mirror'),
    );
    ctx.term.writeln('');
    ctx.status('running');
    running(true);
    const { status } = await ctx.runProgram(ctx.term, {
      elf,
      files,
      args: ['/bin/bash', '-i'],
      cwd: '/root',
      tty: true,
      network,
      onNetEvent: (e) => ctx.setNet(e.split(' ')[0]),
      // Root, because that is who /root belongs to and who a package
      // manager expects to be. Without it the guest is some uid with no
      // passwd entry, and bash greets you with "I have no name!".
      uid: 0,
    });
    ctx.term.writeln(ctx.ansi.dim(`\r\n[bash exited ${status}]`));
    ctx.status('finished', 'good');
  } catch (e) {
    ctx.term.writeln(ctx.ansi.red(`\r\n${e.message}`));
    ctx.status(e.message, 'bad');
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

    // The canvas lives above the terminal for as long as this demo is
    // chosen. It is made here rather than in the page's markup so nothing
    // else has to know the demo exists.
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
        ctx.term.writeln(ctx.ansi.red(`\r\n${e.message}`));
        ctx.status(e.message, 'bad');
      } finally {
        running(false);
        ctx.idle();
        ctx.actions([
          { label: 'Draw 30 frames', primary: true, onClick: () => start(30) },
          { label: 'Draw 200', onClick: () => start(200) },
        ]);
      }
    };

    ctx.actions([
      { label: 'Draw 30 frames', primary: true, onClick: () => start(30) },
      { label: 'Draw 200', onClick: () => start(200) },
    ]);
    ctx.hint('every GL call crosses to this page and runs on WebGL');

    // `?frames=N` starts it straight away, so a link can show someone the
    // thing rather than a button that shows it.
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
      ctx.ansi.red('This one does not draw yet. The X server starts and runs; what stops'),
    );
    ctx.term.writeln(
      ctx.ansi.red('it is the dispatch table, which a browser will not grow past ten'),
    );
    ctx.term.writeln(
      ctx.ansi.red('million entries. A slot is claimed per 4 KiB of guest text and none is'),
    );
    ctx.term.writeln(
      ctx.ansi.red('ever given back, so an X server, Mesa and the LLVM inside it exhaust'),
    );
    ctx.term.writeln(ctx.ansi.red('them. What follows is the run, unedited, as far as it gets.'));
    ctx.term.writeln('');

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
        ctx.term.writeln(ctx.ansi.dim(`  ${renderer.describe().split('|').join('\r\n  ')}`));
        ctx.term.writeln('');

        const tree = await ctx.rootfs('/rootfs/graphics.tar');
        const files = { ...tree.files };
        for (const [path, target] of Object.entries(tree.links)) {
          const abs = target.startsWith('/')
            ? target
            : `${path.slice(0, path.lastIndexOf('/'))}/${target}`.replace(/\/{2,}/g, '/');
          if (files[abs]) files[path] = files[abs];
        }
        // The loader passes no environment, so the bound is written into a
        // script rather than exported around one.
        files['/run.sh'] = new TextEncoder().encode(
          `GEARS_SECONDS=${seconds}\nexport GEARS_SECONDS\n. /gears.sh\n`,
        );

        ctx.idle();
        rule(ctx, `graphics rootfs — ${Object.keys(files).length} files`);
        ctx.status('running');
        const { status } = await ctx.runProgram(ctx.term, {
          elf: files['/bin/sh'],
          files,
          args: ['/bin/sh', '/run.sh'],
          cwd: '/',
          tty: true,
          uid: 0,
          virgl: renderer,
        });
        const ms = (performance.now() - t0).toFixed(0);
        ctx.term.writeln('');
        rule(ctx, `exit ${status} in ${ms} ms`);
        // `presented` is the honest number. vtest has no present -- the X
        // server reads its pixels back and never looks at a screen -- so a
        // run can draw perfectly with a blank canvas, and this says which
        // happened.
        ctx.term.writeln(ctx.ansi.dim(`  ${renderer.glState()}`));
        ctx.status(status === 0 ? 'finished' : `exit ${status}`, status === 0 ? 'good' : 'bad');
      } catch (e) {
        ctx.term.writeln(ctx.ansi.red(`\r\n${e.message}`));
        ctx.status(e.message, 'bad');
      } finally {
        running(false);
        ctx.idle();
        ctx.actions([
          { label: 'Draw for 30s', primary: true, onClick: () => start(30) },
          { label: 'Draw for 2 minutes', onClick: () => start(120) },
        ]);
      }
    };

    ctx.actions([
      { label: 'Draw for 30s', primary: true, onClick: () => start(30) },
      { label: 'Draw for 2 minutes', onClick: () => start(120) },
    ]);
    ctx.hint('Xvfb and glxgears, as Debian built them');
  },
};

export const DEMOS = [upload, busybox, debian, gears, virglGears];
