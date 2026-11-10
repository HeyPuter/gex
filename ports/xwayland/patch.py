#!/usr/bin/env python3
"""Point a host-side Xwayland's I/O at gex.

    ports/xwayland/patch.py sources <wayland-src> <xserver-src>
    ports/xwayland/patch.py xtrans <prefix>

Two calls because xtrans installs its sources into the cross prefix as if they
were headers (the X server compiles them in), so that one has to wait until
xtrans is built.

Called by ports/xwayland/build.sh, after the sources are cloned and before
they are built. Every patch here is one of two kinds:

  * a *rename*: the file gets `#include <gexos.h>` with GEX_OS_RENAME set, so
    the socket calls in that translation unit become the gex ones. Nothing is
    rewritten by hand and no libc symbol is shadowed -- see
    ports/xwayland/gexos.h.

  * a *substitution*: something with no gex equivalent at all -- forking
    xkbcomp, mapping a shared file -- replaced with what this build can do.

Each patch checks for its own marker first, so re-running is free, and asserts
on the text it expects: a silently-skipped patch here is a server that starts
and then does nothing, which is much harder to diagnose than a failed build.
"""
import re
import sys

RENAME = """
/* gex: this file's socket calls go to the gex transport. See
 * ports/xwayland/gexos.h for what that means and why. */
#ifdef __EMSCRIPTEN__
#define GEX_OS_RENAME 1
#include <gexos.h>
#endif
"""


def rename_after(path, anchor, what):
    """Add the rename block after `anchor` in `path`."""
    s = open(path).read()
    if 'GEX_OS_RENAME' in s:
        return False
    assert anchor in s, f'{what}: no {anchor.strip()!r} to anchor to'
    s = s.replace(anchor, anchor + RENAME, 1)
    open(path, 'w').write(s)
    print(f'  {what}: socket calls redirected')
    return True


def patch_xtrans(prefix):
    # Xtrans is compiled into the server from these sources, so the copy in the
    # cross prefix is where the X side of the transport is redirected. Every
    # descriptor it touches is a client connection or the listener.
    rename_after(f'{prefix}/include/X11/Xtrans/Xtranssock.c',
                 '#include <ctype.h>\n', 'Xtranssock.c')


def patch_wayland(src):
    # The compositor is a module in the same Worker as the server, so this
    # connection is a queue and not a socket at all. libwayland reaches it
    # through the same renamed calls; the Worker passes the descriptor in
    # WAYLAND_SOCKET so libwayland never tries to connect to a name.
    rename_after(f'{src}/src/wayland-os.c', '#include "wayland-os.h"\n',
                 'wayland-os.c')
    # connection.c is where the bytes actually move -- `wl_connection_flush`
    # calls sendmsg itself rather than going through wayland-os.c -- so this is
    # the file that decides whether the compositor hears anything at all.
    rename_after(f'{src}/src/connection.c', '#include "wayland-os.h"\n',
                 'connection.c')
    s = open(f'{src}/src/wayland-client.c').read()
    anchor = '#include "wayland-client.h"\n'
    if anchor not in s:
        anchor = '#include "wayland-private.h"\n'
    rename_after(f'{src}/src/wayland-client.c', anchor, 'wayland-client.c')


def patch_ospoll(src):
    # The server's whole idea of waiting. Its poll backend is the one in use
    # (emscripten has no epoll), and every descriptor in it is one of ours.
    rename_after(f'{src}/os/ospoll.c', '#include "list.h"\n', 'ospoll.c')


def patch_shm(src):
    """Pools of pixels, out of this module's memory instead of a file."""
    p = f'{src}/hw/xwayland/xwayland-shm.c'
    s = open(p).read()
    if 'gex_shm_alloc' in s:
        return
    anchor = '#include "xwayland-shm.h"\n'
    assert anchor in s, 'xwayland-shm.c: no xwayland-shm.h include'
    s = s.replace(anchor, anchor + """
/* gex: a wl_shm pool is a file the compositor mmaps, and there is no such file
 * here -- but there is something better. The compositor and the page both read
 * THIS module's memory directly, so a pool is a plain allocation in it and the
 * descriptor is a name for that address: an X window reaches the screen with
 * no copy at all. See ports/xwayland/gexos.h.
 *
 * `close` is deliberately not renamed: a descriptor here is the pool's only
 * owner, and the server closes it as soon as it has handed it to wl_shm. */
#ifdef __EMSCRIPTEN__
#include <gexos.h>
#define mmap(addr, len, prot, flags, fd, off) gex_shm_map(fd, len)
#define munmap(addr, len) gex_shm_unmap(addr, len)
#define ftruncate(fd, len) gex_shm_truncate(fd, len)
#endif
""", 1)
    # The allocation itself is one call site, and renaming
    # `os_create_anonymous_file` with a macro would rename the static
    # definition of it in this same file too.
    old = '    fd = os_create_anonymous_file(size);'
    assert old in s, 'xwayland-shm.c: the pool allocation did not match'
    s = s.replace(old, """#ifdef __EMSCRIPTEN__
    fd = gex_shm_alloc(size);
#else
    fd = os_create_anonymous_file(size);
#endif""", 1)
    open(p, 'w').write(s)
    print('  xwayland-shm.c: pools are memory, not files')


def patch_xkb(src):
    """A keymap without forking xkbcomp."""
    p = f'{src}/xkb/ddxLoad.c'
    s = open(p).read()
    if 'GEX_XKM' in s:
        return
    # The server compiles a keymap by writing a description to xkbcomp's stdin
    # and reading the .xkm file back. There is no fork here and nothing to exec,
    # so the .xkm is compiled at build time by the host's xkbcomp -- from the
    # same evdev/pc105/us description the compositor sends -- and embedded in
    # the module. Both halves of the dance are replaced: opening the compiled
    # file finds the embedded one, and "compiling" is a no-op.
    m = re.search(r'static FILE \*\nXkbDDXOpenConfigFile\(.*?\n\}\n', s, re.S)
    assert m, 'ddxLoad.c: XkbDDXOpenConfigFile did not match'
    ours = """#ifdef __EMSCRIPTEN__
/* GEX_XKM: the keymap this build uses, compiled by the host's xkbcomp and
 * embedded in the module. See ports/xwayland/build.sh. */
#define GEX_XKM_PATH "/xkb/gex.xkm"

static FILE *
XkbDDXOpenConfigFile(const char *mapName, char *fileNameRtrn, int fileNameRtrnLen)
{
    (void) mapName;
    if (fileNameRtrn != NULL && fileNameRtrnLen > 0)
        strlcpy(fileNameRtrn, GEX_XKM_PATH, fileNameRtrnLen);
    return fopen(GEX_XKM_PATH, "rb");
}
#else
"""
    s = s[:m.start()] + ours + m.group(0) + '#endif /* __EMSCRIPTEN__ */\n' + s[m.end():]

    # "Compiling" the keymap: already done, at build time.
    m = re.search(r'( *)else if \(!XkbDDXCompileKeymapByNames\(xkb, names, want, need,\n'
                  r' *nameRtrn, nameRtrnLen\)\) \{', s)
    assert m, 'ddxLoad.c: the compile call did not match'
    s = s[:m.start()] + ('#ifdef __EMSCRIPTEN__\n'
                         '    else if (0) {   /* GEX_XKM: compiled at build time */\n'
                         '#else\n' + m.group(0) + '\n#endif') + s[m.end():]

    # And the step before it: turning "evdev, pc105, us" into a list of
    # component files to compile. That reads xkeyboard-config's rules files,
    # which this build does not ship either -- the compiled keymap already has
    # every component in it. So the names are replaced by one that exists only
    # to satisfy the loader's "some component was asked for" check.
    old = '''    if (XkbRMLVOtoKcCGST(dev, rmlvo, &kccgst)) {'''
    assert old in s, 'ddxLoad.c: XkbRMLVOtoKcCGST call did not match'
    s = s.replace(old, '''#ifdef __EMSCRIPTEN__
    /* GEX_XKM: no rules files to read; the keymap is already compiled. */
    kccgst.keycodes = strdup("gex");
    if (1) {
#else
    if (XkbRMLVOtoKcCGST(dev, rmlvo, &kccgst)) {
#endif''', 1)

    # The loader unlinks the .xkm once it has read it, which would throw the
    # embedded one away the first time a keyboard appears.
    old = """    fclose(file);
    (void) unlink(fileName);
    return (need | want) & (~missing);"""
    assert old in s, 'ddxLoad.c: the unlink after loading did not match'
    s = s.replace(old, """    fclose(file);
#ifndef __EMSCRIPTEN__
    /* GEX_XKM: the embedded keymap is read again for every keyboard. */
    (void) unlink(fileName);
#endif
    return (need | want) & (~missing);""", 1)
    open(p, 'w').write(s)
    print('  ddxLoad.c: keymap comes from the embedded .xkm')


def patch_repeat(src):
    """The page's keyboard repeats keys, not the server's timer."""
    p = f'{src}/xkb/ddxCtrls.c'
    s = open(p).read()
    if 'GEX_PAGE_REPEATS' in s:
        return
    # The server repeats a held key from a timer it checks before it reads
    # input (os/WaitFor.c: check_timers, then poll). Busy past the repeat
    # delay -- drawing a terminal's first character took longer than 600 ms
    # -- it repeated keys whose release had already come: "ls" typed as
    # "llls". A browser repeats a held key itself, as the system's settings
    # say, and stops at the release whenever the page reads it; the
    # compositor passes each repeat on as a press of a key already down,
    # which is made a repeat event below (XkbDDXUsesSoftRepeat 0: no timer).
    old = """int
XkbDDXUsesSoftRepeat(DeviceIntPtr pXDev)
{
    return 1;
}"""
    assert old in s, 'ddxCtrls.c: no XkbDDXUsesSoftRepeat to patch'
    s = s.replace(old, """/* GEX_PAGE_REPEATS: the page's keyboard repeats (ports/xwayland/patch.py). */
int
XkbDDXUsesSoftRepeat(DeviceIntPtr pXDev)
{
#ifdef __EMSCRIPTEN__
    return 0;
#else
    return 1;
#endif
}""")
    open(p, 'w').write(s)
    print('  ddxCtrls.c: the page repeats keys')


def patch_repeat_press(src):
    """The press of a key that is down repeats it (patch_repeat)."""
    # The press of a key already down is that repeat: queued as XKB's own
    # repeats are (key_repeat set), since XKB drops a second plain press of
    # a key it has down. The dix makes none for a key that does not repeat
    # or with autorepeat off.
    p = f'{src}/hw/xwayland/xwayland-input.c'
    s = open(p).read()
    if 'GEX_PAGE_REPEATS' in s:
        return
    old = """    end = (uint32_t *) ((char *) xwl_seat->keys.data + xwl_seat->keys.size);
    for (k = xwl_seat->keys.data; k < end; k++) {
        if (*k == key)
            *k = *--end;
    }
    xwl_seat->keys.size = (char *) end - (char *) xwl_seat->keys.data;
    if (state) {
        k = wl_array_add(&xwl_seat->keys, sizeof *k);
        *k = key;
    }

    QueueKeyboardEvents(xwl_seat->keyboard,
                        state ? KeyPress : KeyRelease, key + 8);
"""
    assert old in s, 'xwayland-input.c: keyboard_handle_key did not match'
    s = s.replace(old, """    int down = 0;

    end = (uint32_t *) ((char *) xwl_seat->keys.data + xwl_seat->keys.size);
    for (k = xwl_seat->keys.data; k < end; k++) {
        if (*k == key) {
            *k = *--end;
            down = 1;
        }
    }
    xwl_seat->keys.size = (char *) end - (char *) xwl_seat->keys.data;
    if (state) {
        k = wl_array_add(&xwl_seat->keys, sizeof *k);
        *k = key;
    }

#ifdef __EMSCRIPTEN__
    /* GEX_PAGE_REPEATS (ports/xwayland/patch.py): a press of a key already
     * down is the page's repeat of it */
    if (state && down) {
        int i, n = GetKeyboardEvents(InputEventList, xwl_seat->keyboard, KeyPress, key + 8);

        for (i = 0; i < n; i++) {
            if (InputEventList[i].any.type == ET_KeyPress)
                InputEventList[i].device_event.key_repeat = TRUE;
            mieqEnqueue(xwl_seat->keyboard, &InputEventList[i]);
        }
        return;
    }
#endif
    QueueKeyboardEvents(xwl_seat->keyboard,
                        state ? KeyPress : KeyRelease, key + 8);
""")
    if '#include <eventstr.h>' not in s:
        s = s.replace('#include <inpututils.h>\n', '#include <inpututils.h>\n#include <eventstr.h>\n', 1)
    open(p, 'w').write(s)
    print('  xwayland-input.c: a press of a key that is down repeats it')


def patch_noop_casts(src):
    """Typed no-ops, because wasm checks what a function pointer is.

    The X server has one `NoopDDA(void)` that it casts to whatever function
    pointer needs to do nothing -- a colormap destructor, a pointer-control
    callback, a block handler, a resource destructor. Calling through such a
    cast is undefined behaviour that x86 tolerates and wasm traps on:
    "function signature mismatch", from a stack deep inside device
    initialisation, before the server has said anything at all.

    So each cast gets a stub of the right shape. They go in after the file's
    own includes, which is where the types they name become available.
    """
    stubs = {
        'dix/devices.c': [
            ('(PtrCtrlProcPtr) NoopDDA', 'GexNoopPtrCtrl',
             'static void\nGexNoopPtrCtrl(DeviceIntPtr dev, PtrCtrl *ctrl)\n{\n}\n'),
        ],
        'fb/fbscreen.c': [
            ('(void (*)(ColormapPtr)) NoopDDA', 'GexNoopDestroyColormap',
             'static void\nGexNoopDestroyColormap(ColormapPtr map)\n{\n}\n'),
            ('(void (*)(ColormapPtr, int, xColorItem *)) NoopDDA', 'GexNoopStoreColors',
             'static void\nGexNoopStoreColors(ColormapPtr map, int n, xColorItem *defs)\n{\n}\n'),
        ],
        'mi/miscrinit.c': [
            ('(ScreenBlockHandlerProcPtr) NoopDDA', 'GexNoopBlockHandler',
             'static void\nGexNoopBlockHandler(ScreenPtr screen, void *timeout)\n{\n}\n'),
            ('(ScreenWakeupHandlerProcPtr) NoopDDA', 'GexNoopWakeupHandler',
             'static void\nGexNoopWakeupHandler(ScreenPtr screen, int result)\n{\n}\n'),
        ],
        'dix/resource.c': [
            ('(DeleteType) NoopDDA', 'GexNoopDelete',
             'static int\nGexNoopDelete(void *value, XID id)\n{\n    return Success;\n}\n'),
        ],
    }
    for path, edits in stubs.items():
        p = f'{src}/{path}'
        s = open(p).read()
        if 'GexNoop' in s:
            continue
        text = ''
        for cast, name, stub in edits:
            assert cast in s, f'{path}: {cast} did not match'
            s = s.replace(cast, name)
            text += '\n' + stub
        # Placed at the end of the last top-level definition before the first
        # use. Not "after the last include": some of these files have their
        # includes inside an `#ifdef` for an extension this build turns off, and
        # a stub that lands in there is compiled away with it. The close brace
        # of a function is always at file scope and always compiled.
        first_use = min(s.index(name) for _, name, _ in edits)
        last = s.rfind('\n}\n', 0, first_use)
        assert last > 0, f'{path}: nowhere at file scope to put a stub'
        last += 3
        s = (s[:last]
             + '\n/* gex: wasm checks what a function pointer is, and the X server\'s\n'
               ' * one-size-fits-all NoopDDA is not any of these shapes. See\n'
               ' * ports/xwayland/patch.py. */\n'
             + text + s[last:])
        open(p, 'w').write(s)
        print(f'  {path}: {len(edits)} NoopDDA cast(s) given a shape')


def patch_input(src):
    """The evdev numbering, without the kernel's headers."""
    p = f'{src}/hw/xwayland/xwayland-input.c'
    s = open(p).read()
    if 'input-event-codes.h' not in s:
        s = s.replace('#include <linux/input.h>',
                      '#include <linux/input-event-codes.h>', 1)
        assert 'input-event-codes.h' in s, 'xwayland-input.c: the input include did not match'
        open(p, 'w').write(s)
        print('  xwayland-input.c: evdev codes only')


def patch_meson(src):
    """Build the transport into the server."""
    p = f'{src}/hw/xwayland/meson.build'
    s = open(p).read()
    if 'gexos.c' in s:
        # A tree patched before gexgl.c was part of it.
        if 'gexgl.c' not in s:
            s = s.replace("""    'gexos.c',""", """    'gexos.c',
    'gexgl.c',""", 1)
            open(p, 'w').write(s)
            print('  xwayland/meson.build: gexgl.c is part of the server')
        return
    s = s.replace("""    '../../mi/miinitext.c',""",
                  """    'gexos.c',
    'gexgl.c',
    '../../mi/miinitext.c',""", 1)
    assert 'gexos.c' in s, 'xwayland/meson.build: the source list did not match'

    # And the last word on the pthread pool.
    #
    # meson appends `-sPTHREAD_POOL_SIZE=<c_thread_count>` (4 by default) after
    # everything from the cross file, once per dependency, and the last setting
    # is the one emcc keeps -- so the module preallocates four Workers and then
    # blocks forever on "still waiting on run dependencies: loading-workers"
    # before `main` runs. This server never makes a thread: it is one Worker
    # that blocks, like a guest process. A target's own link_args come after the
    # dependencies', which is the only place that can say so.
    s = s.replace("""xwayland_server = executable(
    'Xwayland',
    srcs,""", """xwayland_server = executable(
    'Xwayland',
    srcs,
    link_args: [ '-sPTHREAD_POOL_SIZE=0' ],""", 1)
    assert 'PTHREAD_POOL_SIZE=0' in s, 'xwayland/meson.build: the executable did not match'
    open(p, 'w').write(s)
    print('  xwayland/meson.build: gexos.c and gexgl.c are part of the server, and no thread pool')


def main():
    mode = sys.argv[1]
    print('patching the transport:')
    if mode == 'xtrans':
        patch_xtrans(sys.argv[2])
        return
    wayland, xserver = sys.argv[2], sys.argv[3]
    patch_wayland(wayland)
    patch_ospoll(xserver)
    patch_shm(xserver)
    patch_xkb(xserver)
    patch_repeat(xserver)
    patch_repeat_press(xserver)
    patch_noop_casts(xserver)
    patch_input(xserver)
    patch_meson(xserver)


if __name__ == '__main__':
    main()
