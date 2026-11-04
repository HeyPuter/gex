/* A program's or DLL's C++ constructors (wasm's __wasm_call_ctors, whose
 * export gexcc-win renames so the loader leaves them alone) run from its entry
 * point, after the DLLs it imports are loaded and initialised, as the
 * Windows CRT runs them. */
typedef int BOOL; typedef unsigned int DWORD; typedef void *HANDLE;
extern void __wasm_call_ctors(void);
int mainCRTStartup(void);
int wmainCRTStartup(void);
BOOL __wine_spec_dll_entry(HANDLE, DWORD, void *);  /* winecrt0's: DllMain */
#ifdef ONLY_main
int __gexw_mainCRTStartup(void) { __wasm_call_ctors(); return mainCRTStartup(); }
#endif
#ifdef ONLY_wmain
int __gexw_wmainCRTStartup(void) { __wasm_call_ctors(); return wmainCRTStartup(); }
#endif
#ifdef ONLY_dll
BOOL __gexw_DllMainCRTStartup(HANDLE inst, DWORD reason, void *reserved)
{
    if (reason == 1 /* DLL_PROCESS_ATTACH */) __wasm_call_ctors();
    return __wine_spec_dll_entry(inst, reason, reserved);
}
#endif
