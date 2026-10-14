/* PROVIDES msvcp120.dll i386
 *
 * _Thrd_current without its two system calls. MSVC 2013's msvcp120 makes a
 * real handle to the current thread (DuplicateHandle of GetCurrentThread),
 * closes it again at once, and returns the closed handle's value with
 * GetCurrentThreadId: { eax = a handle that is no longer open, edx = id }.
 * Only the id is usable (_Thrd_equal compares ids). Noita asks about 1700
 * times a second on its main thread (std::this_thread::get_id), and in the
 * page each NtDuplicateObject and NtClose is a trip through the NT layer.
 * When DuplicateHandle fails the DLL returns a handle of 0, which is what
 * this returns every time. Wine's own msvcp120 (dlls/msvcp90/misc.c,
 * thread_current) does the same: the same duplicate-then-close pair, the
 * same 8-byte struct in EDX:EAX on i386; on W3 each of the two is a call
 * into the wineserver (128 a world frame, 2.2 ms, docs/wine-overhead.md
 * F1), so this binds there too. cdecl, no arguments. */

static GEX_PROVIDER(p_thrd_current) {
    (void)depth;
    (void)need;
    /* The TEB (FS base); its ClientId.UniqueThread at 0x24 is
     * GetCurrentThreadId on i386. */
    uint32_t teb = (uint32_t)GEX_C64(GEX_CTX_TPIDR);
    gp_set(ctx, R_EDX, GW(teb + 0x24));
    return gp_return_eax(ctx, ret, 0, 0);
}

GEX_PROVIDE(p_thrd_current, "_Thrd_current")
