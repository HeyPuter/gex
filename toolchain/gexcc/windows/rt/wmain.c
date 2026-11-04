/* The CRT calls wmain(argc, argv), and a call of another type traps: a
 * program's wmain(void) or wmain(argc, argv, envp), which gexcc-win compiles
 * as __gexw_wmain0 or __gexw_wmain3, is called from one of these. */
typedef unsigned short WCHAR;
#ifdef ONLY_0
int __gexw_wmain0(void);
int wmain(int argc, WCHAR **argv) { return __gexw_wmain0(); }
#endif
#ifdef ONLY_3
WCHAR **_get_initial_wide_environment(void);
int __gexw_wmain3(int argc, WCHAR **argv, WCHAR **envp);
int wmain(int argc, WCHAR **argv) { return __gexw_wmain3(argc, argv, _get_initial_wide_environment()); }
#endif
