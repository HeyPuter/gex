/* A --dir's large files are lazy under run-node (read as the guest reads
 * them, as a page's rest bundle is): each must behave as a bundled file.
 * Run as `lazyfile DIR`, with DIR holding child (this program, > 64 KiB),
 * big.bin (BIG bytes of pattern(i)) and over.bin (anything > 64 KiB). */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define BIG (3 << 20)
static unsigned char pattern(long i) { return (unsigned char)(i * 131 + (i >> 12)); }
static int fails;
static void check(int ok, const char *what) {
	printf("%s %s\n", ok ? "ok  " : "FAIL", what);
	if (!ok) fails++;
}

int main(int argc, char **argv) {
	if (argc > 1 && !strcmp(argv[1], "child")) {
		printf("child ran\n");
		return 7;
	}
	if (argc < 2) return 2;
	char p[512];
	setvbuf(stdout, 0, _IONBF, 0);

	/* exec of a lazy ELF */
	snprintf(p, sizeof p, "%s/child", argv[1]);
	pid_t pid = fork();
	if (pid == 0) {
		execl(p, p, "child", (char *)0);
		_exit(99);
	}
	int st = 0;
	waitpid(pid, &st, 0);
	check(WIFEXITED(st) && WEXITSTATUS(st) == 7, "exec of a lazy program");

	/* size, and mmap of a lazy file */
	snprintf(p, sizeof p, "%s/big.bin", argv[1]);
	struct stat sb;
	check(stat(p, &sb) == 0 && sb.st_size == BIG, "stat size of a lazy file");
	int fd = open(p, O_RDONLY);
	unsigned char *m = mmap(0, BIG, PROT_READ, MAP_PRIVATE, fd, 0);
	long bad = m == MAP_FAILED ? -1 : 0;
	for (long i = 0; bad == 0 && i < BIG; i++)
		if (m[i] != pattern(i)) bad = i + 1;
	check(bad == 0, "mmap of a lazy file reads its bytes");
	if (m != MAP_FAILED) munmap(m, BIG);
	close(fd);

	/* a write into it, then reads and a map of the result */
	fd = open(p, O_RDWR);
	check(fd >= 0 && pwrite(fd, "GEX!", 4, 2000000) == 4, "pwrite into a lazy file");
	unsigned char *b = malloc(BIG);
	check(pread(fd, b, BIG, 0) == BIG, "pread the whole file after it");
	bad = 0;
	for (long i = 0; bad == 0 && i < BIG; i++) {
		unsigned char want = i >= 2000000 && i < 2000004 ? "GEX!"[i - 2000000] : pattern(i);
		if (b[i] != want) bad = i + 1;
	}
	check(bad == 0, "the write and every other byte read back");
	m = mmap(0, BIG, PROT_READ, MAP_SHARED, fd, 0);
	check(m != MAP_FAILED && !memcmp(m + 2000000, "GEX!", 4) && m[BIG - 1] == pattern(BIG - 1), "a map after the write");
	close(fd);

	/* a new file renamed over a lazy one */
	char tmp[512], over[512];
	snprintf(tmp, sizeof tmp, "%s/over.tmp", argv[1]);
	snprintf(over, sizeof over, "%s/over.bin", argv[1]);
	fd = open(tmp, O_CREAT | O_WRONLY | O_TRUNC, 0644);
	check(fd >= 0 && write(fd, "replaced\n", 9) == 9, "write a new file");
	close(fd);
	check(rename(tmp, over) == 0, "rename it over a lazy file");
	char got[64] = {0};
	fd = open(over, O_RDONLY);
	check(fd >= 0 && read(fd, got, sizeof got) == 9 && !strcmp(got, "replaced\n"), "the renamed file's bytes, not the lazy one's");
	close(fd);

	printf("%d failed\n", fails);
	return fails != 0;
}
