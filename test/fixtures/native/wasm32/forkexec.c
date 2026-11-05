/* A forked child's execve: the new program is the same process (its pid,
 * its parent's wait), with what an exec keeps -- descriptors not marked
 * close-on-exec, ignored signals, the blocked mask, the working directory,
 * the environment it was given -- and without what it drops: a
 * close-on-exec descriptor, a caught signal's handler. Then a grandchild
 * forked and exec'd by the exec'd child. One line per check, compared with
 * native. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void on_usr2(int s) { (void)s; }

static int exec_side(char **argv)
{
	/* argv: self "exec" PID DEPTH */
	int ok = 1;
	char line[256];
	char cwd[256];
	sigset_t m;
	struct sigaction sa;
	ok &= atoi(argv[2]) == getpid();
	ok &= getcwd(cwd, sizeof cwd) && strstr(cwd, "forkexec-dir") != NULL;
	ok &= getenv("FORKEXEC") && strcmp(getenv("FORKEXEC"), "kept") == 0;
	sigaction(SIGUSR1, NULL, &sa);
	ok &= sa.sa_handler == SIG_IGN;
	sigaction(SIGUSR2, NULL, &sa);
	ok &= sa.sa_handler == SIG_DFL;
	sigprocmask(SIG_BLOCK, NULL, &m);
	ok &= sigismember(&m, SIGTERM) == 1;
	ok &= fcntl(6, F_GETFD) < 0 && errno == EBADF;
	snprintf(line, sizeof line, "depth %s: %s\n", argv[3], ok ? "kept and dropped what an exec does" : "FAIL");
	if (write(5, line, strlen(line)) != (ssize_t)strlen(line))
		return 3;
	if (atoi(argv[3]) == 0) {
		/* and one more generation, forked from the exec'd program */
		pid_t c = fork();
		if (c == 0) {
			char pid[16];
			snprintf(pid, sizeof pid, "%d", (int)getpid());
			execl(argv[0], argv[0], "exec", pid, "1", (char *)NULL);
			_exit(127);
		}
		int s = 0;
		waitpid(c, &s, 0);
		if (!WIFEXITED(s) || WEXITSTATUS(s) != 7)
			return 4;
	}
	return 7;
}

int main(int argc, char **argv)
{
	if (argc > 3 && strcmp(argv[1], "exec") == 0)
		return exec_side(argv);
	setvbuf(stdout, NULL, _IONBF, 0);
	char self[512];
	if (argv[0][0] == '/') snprintf(self, sizeof self, "%s", argv[0]);
	else if (!getcwd(self, 300)) return 2;
	else snprintf(self + strlen(self), sizeof self - strlen(self), "/%s", argv[0]);
	mkdir("forkexec-dir", 0755);
	int p[2];
	if (pipe(p) != 0) return 2;
	pid_t c = fork();
	if (c == 0) {
		close(p[0]);
		dup2(p[1], 5);
		close(p[1]);
		/* a close-on-exec descriptor the exec drops */
		if (fcntl(5, F_DUPFD_CLOEXEC, 6) != 6) _exit(120);
		signal(SIGUSR1, SIG_IGN);
		signal(SIGUSR2, on_usr2);
		sigset_t m;
		sigemptyset(&m);
		sigaddset(&m, SIGTERM);
		sigprocmask(SIG_BLOCK, &m, NULL);
		if (chdir("forkexec-dir") != 0) _exit(121);
		char pid[16];
		snprintf(pid, sizeof pid, "%d", (int)getpid());
		char *envp[] = { "FORKEXEC=kept", NULL };
		char *av[] = { self, "exec", pid, "0", NULL };
		execve(self, av, envp);
		_exit(127);
	}
	close(p[1]);
	char buf[512];
	size_t n = 0;
	ssize_t r;
	while ((r = read(p[0], buf + n, sizeof buf - 1 - n)) > 0)
		n += r;
	buf[n] = 0;
	fputs(buf, stdout);
	int s = 0;
	pid_t w = waitpid(c, &s, 0);
	printf("parent: waited for its child: %s, exit %d\n", w == c ? "yes" : "no", WIFEXITED(s) ? WEXITSTATUS(s) : -1);
	rmdir("forkexec-dir");
	return 0;
}
