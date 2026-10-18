/* A stand-in for Hades' BasecampBugReporter.Native.dll (its crash and bug
 * reporter), for runs with no CLR: the real DLL is mixed-mode C++/CLI, so
 * loading it starts mscoree, which stalls in the loader with no runtime
 * installed. The engine imports it statically. This one reports nothing.
 * The exports are the MSVC-mangled names (basecamp-i386.def,
 * basecamp-x64.def). Built by build.sh. */
#ifdef __i386__
#define API __attribute__((stdcall))
#else
#define API
#endif

/* HandleAssert(file, expr, line): nonzero would ask the caller to break. */
API int bb_HandleAssert(const char *a, const char *b, int c) { (void)a; (void)b; (void)c; return 0; }
API void bb_Initialize(const char *a, int b, const char *c, const char *d, const char *e) { (void)a; (void)b; (void)c; (void)d; (void)e; }
API void bb_ReportBug(void *info) { (void)info; }
/* Whether the report is done: there is none. */
API _Bool bb_WaitReportComplete(void) { return 1; }

int __attribute__((stdcall)) DllMain(void *h, unsigned long r, void *p) { (void)h; (void)r; (void)p; return 1; }
