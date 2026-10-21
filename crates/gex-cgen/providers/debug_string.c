/* PROVIDES kernel32.dll,kernelbase.dll i386
 *
 * OutputDebugStringA/W with no debugger attached: both bound, and neither
 * raises. Windows (and
 * Wine) raise DBG_PRINTEXCEPTION_C so a debugger can take the string, catch
 * it themselves when none does, then look for a DBWIN monitor (a mutex, an
 * event and a section). A gex process has no debugger and its session no
 * monitor, so the call has no effect the guest could see beyond those
 * lookups failing. The raise cost a nested exception dispatch per string
 * (a program may write several at start-up, from under its main loop).
 * Wine's kernel32 has an OutputDebugStringA of its own (the DBWIN side),
 * which raises through kernelbase's RaiseException, so both are bound.
 * Both are stdcall with one argument; neither returns a value. */

static GEX_PROVIDER(p_output_debug_string) {
    (void)depth;
    (void)need;
    return gp_return(ctx, ret, 4);
}

GEX_PROVIDE(p_output_debug_string, "OutputDebugStringA")
GEX_PROVIDE(p_output_debug_string, "OutputDebugStringW")
