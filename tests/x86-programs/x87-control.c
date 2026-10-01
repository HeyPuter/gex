/* The x87 control word as the hardware keeps it: whatever fldcw, fldenv,
 * frstor or fxrstor loads, fnstcw, fnstenv, fnsave and fxsave report bits 7
 * and 13-15 as zero and reserved bit 6 as one. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static unsigned char area[512] __attribute__((aligned(16)));

int main(void) {
    static const uint16_t vals[] = {0x0000, 0xffff, 0x023f, 0x027f, 0x137f, 0xe000, 0x1000, 0x00c0, 0x0f3f};
    for (unsigned i = 0; i < sizeof vals / sizeof *vals; i++) {
        uint16_t in = vals[i], cw, env[14], save[54];
        __asm__ volatile("fninit\n fldcw %1\n fnstcw %0" : "=m"(cw) : "m"(in));
        __asm__ volatile("fnstenv %0" : "=m"(env));
        __asm__ volatile("fldcw %1\n fnsave %0" : "=m"(save) : "m"(in));
        uint16_t fx;
        __asm__ volatile("fninit\n fldcw %1\n fxsave %2\n fninit\n fxrstor %2\n fnstcw %0" : "=m"(fx) : "m"(in), "m"(area) : "memory");
        /* Loading through the environment: fldenv of an image holding `in`. */
        uint16_t img[14], back;
        __asm__ volatile("fninit\n fnstenv %0" : "=m"(img));
        img[0] = in;
        __asm__ volatile("fldenv %1\n fnstcw %0\n fninit" : "=m"(back) : "m"(img));
        printf("%04x: fnstcw %04x fnstenv %04x fnsave %04x fxsave %04x fxrstor %04x fldenv %04x\n", in, cw, env[0],
               save[0], *(uint16_t *)area, fx, back);
    }
    return 0;
}
