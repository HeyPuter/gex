/* mremap-shared: a shared file mapping grown with mremap, as sqlite grows
 * its mapping of a database (unixRemapfile, an old length of the file's size,
 * short of a page): the grown part shows the file, writes to the file show
 * in the mapping at its new address, nothing lands at the old one, and the
 * file keeps them past a shrink and the unmap. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
static int bad;
#define CHECK(c, ...) do { if (!(c)) { printf("FAIL " __VA_ARGS__); printf("\n"); bad = 1; } } while (0)
int main(void) {
  const char *f = "mremap-shared.db";
  unlink(f);
  int fd = open(f, O_RDWR | O_CREAT, 0600);
  char b[0x4000];
  memset(b, 'A', 0x800);
  pwrite(fd, b, 0x800, 0);
  // the database's 0x800 bytes, mapped, with a page held right after it
  // so that the growth has to move
  unsigned char *hold = mmap(0, 0x2000, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  unsigned char *p = mmap(hold, 0x800, PROT_READ, MAP_SHARED | MAP_FIXED, fd, 0);
  CHECK(p != MAP_FAILED && p[0] == 'A', "first map");
  // sqlite: grow the file, write its new pages, remap
  ftruncate(fd, 0xe000);
  memset(b, 'B', 0x1000);
  pwrite(fd, b, 0x1000, 0x3000);
  unsigned char *q = mremap(p, 0x800, 0xe000, MREMAP_MAYMOVE);
  CHECK(q != MAP_FAILED, "mremap");
  CHECK(q != p, "it moved");
  CHECK(q[0] == 'A' && q[0x7ff] == 'A', "the old part moved: %c", q[0]);
  CHECK(q[0x3000] == 'B' && q[0x3fff] == 'B', "the grown part shows the file: %d", q[0x3000]);
  CHECK(q[0x5000] == 0, "past what was written: zero");
  // something else at the old address
  unsigned char *z = p;
  if (q != p) {
    z = mmap(p, 0x1000, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if (z != MAP_FAILED) memset(z, 'Z', 0x1000);
  }
  // writes to the file afterwards, at the start and in the grown part
  memset(b, 'C', 0x800);
  pwrite(fd, b, 0x800, 0);
  memset(b, 'D', 0x1000);
  pwrite(fd, b, 0x1000, 0x6000);
  CHECK(q[0] == 'C', "a write at the start shows in the moved mapping: %c", q[0]);
  CHECK(q[0x6000] == 'D', "a write in the grown part shows: %d", q[0x6000]);
  if (q != p && z == p) CHECK(z[0] == 'Z' && z[0x7ff] == 'Z', "nothing written at the old address: %c", z[0]);
  // shrink it again, then unmap: the file keeps what was written
  unsigned char *r = mremap(q, 0xe000, 0x4000, 0);
  CHECK(r == q, "shrink in place");
  memset(b, 'E', 0x1000);
  pwrite(fd, b, 0x1000, 0x2000);
  CHECK(r[0x2000] == 'E', "a write after the shrink shows");
  pwrite(fd, b, 0x1000, 0x8000);
  munmap(r, 0x4000);
  if (q != p && z == p) munmap(z, 0x1000);
  munmap(hold + 0x1000, 0x1000);
  char c[4] = {0};
  pread(fd, c, 1, 0); pread(fd, c + 1, 1, 0x3000); pread(fd, c + 2, 1, 0x6000); pread(fd, c + 3, 1, 0x8000);
  CHECK(!memcmp(c, "CBDE", 4), "the file after the unmap: %.4s", c);
  close(fd);
  unlink(f);
  printf(bad ? "mremap-shared: FAIL\n" : "mremap-shared: ok\n");
  return bad;
}
