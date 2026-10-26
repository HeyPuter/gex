/* The GexA64Icall pass's {function, thunk} pairs (section gexa_entries),
 * for the shell to bind (cgen/libprov.rs `gex_libprov_installed`), exported
 * as __gexa_entries: their bounds. Linked into a library built with the
 * pass (an undefined weak bound would be a GOT.mem import). */
extern char __start_gexa_entries[], __stop_gexa_entries[];
void *__gexa_entries[2] = { __start_gexa_entries, __stop_gexa_entries };
