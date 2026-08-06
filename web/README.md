# web — the demo page

A Vite app that runs AArch64 Linux programs in the browser. Three demos:

1. **Run your own binary.** Drop in a statically linked aarch64 ELF. There is
   no ahead-of-time step in a browser, so the page ships a *seed* module
   (`gexc seed`, 1.4 KB) that translates nothing and lets the runtime
   translator produce the whole program on demand.
2. **A BusyBox userspace.** A root filesystem and a shell on top of it, with
   real `fork`, `exec` and pipes.
3. **Debian, with `apt`.** `apt-get update` fetches and parses the real
   bookworm index. Networking goes over Wisp, because a page cannot open a
   TCP socket.

## Running it

    ./build-assets.sh      # the two wasm modules and the rootfs tars
    npm install
    npm run dev

`build-assets.sh --debian` also builds the Debian image, which needs
`debootstrap` and root.

## How fork works here

There is one address space: a guest address is a byte offset into the page's
single `WebAssembly.Memory`, and a page gets one memory. So `fork` is done by
checkpoint — the parent's memory, kernel and dispatch state are copied aside,
the child runs in place, and when the child exits the parent is restored with
the child's pid already in `x0`.

That buys a whole userspace in a page. What it costs is concurrency: the
parent does not run while the child does. For `fork` then `wait`, which is
every shell, that is invisible. For a parent that forks and keeps working it
is not, and it shows up as the parent simply not running until the child is
finished.
