/* gexcc runtime: `int main(int argc, char **argv, char **envp)`. clang
 * names the two-argument main __main_argc_argv (crt1 calls it) and
 * main(void) __original_main, but leaves the three-argument form as plain
 * `main`. The driver links this object when a program's objects define
 * `main` and neither of the others; it is not in libgexcc.a, where it would
 * be pulled for main(void) programs too. */
extern char **environ;
int main(int, char **, char **);

int __main_argc_argv(int argc, char **argv)
{
	return main(argc, argv, environ);
}
