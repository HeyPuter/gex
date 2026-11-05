// Code that ends in the last bytes of an executable mapping: a JIT's chunk
// (Mono's) can end with a 5-byte jmp in its final bytes. Each case puts
// `mov eax, imm32; ret` so that the ret is the mapping's last byte, then
// shorter tails down to a lone `ret`.
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
int main(void) {
  long sum = 0;
  for (int tail = 1; tail <= 8; tail++) {
    unsigned char *p = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return 1;
    memset(p, 0xcc, 4096);
    unsigned char *f;
    if (tail >= 6) {
      f = p + 4096 - 6;
      unsigned char code[6] = {0xb8, (unsigned char)(40 + tail), 0, 0, 0, 0xc3};
      memcpy(f, code, 6);
    } else {
      // a short jmp back from the tail to a body earlier in the page, then the tail is a jmp rel32 ending at the last byte
      f = p + 4096 - 5;
      unsigned char *body = p + 64;
      unsigned char code[6] = {0xb8, (unsigned char)(40 + tail), 0, 0, 0, 0xc3};
      memcpy(body, code, 6);
      int rel = (int)(body - (f + 5));
      f[0] = 0xe9; memcpy(f + 1, &rel, 4);
    }
    int (*fn)(void) = (int (*)(void))f;
    sum += fn();
    munmap(p, 4096);
  }
  printf("exec-tail sum %ld\n", sum);
  return 0;
}
