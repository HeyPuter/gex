/* A small register VM, for the shape an interpreter has.
 *
 * Dispatch is a computed goto, so the hot loop is an indirect branch that
 * gex cannot see the target of -- which is what Lua, CPython and every other
 * bytecode interpreter look like from the translator's side, and a shape that
 * defeats anything relying on statically known successors.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

enum { OP_HALT, OP_LOADI, OP_MOV, OP_ADD, OP_SUB, OP_MUL, OP_XOR, OP_SHR,
       OP_JMPNZ, OP_JMP, OP_LOAD, OP_STORE, OP_CMPLT, OP_NOPS };

struct insn { uint8_t op, a, b, c; int32_t imm; };

#define MEMSZ 4096

static uint64_t run(const struct insn *code, uint64_t iterations) {
    static const void *table[OP_NOPS] = {
        &&halt, &&loadi, &&mov, &&add, &&sub, &&mul, &&xor_, &&shr,
        &&jmpnz, &&jmp, &&load, &&store, &&cmplt,
    };
    uint64_t r[16] = {0}, mem[MEMSZ] = {0};
    r[15] = iterations;
    const struct insn *ip = code;
#define NEXT() goto *table[(++ip)->op]
    goto *table[ip->op];
halt:
    return r[0] ^ r[1] ^ mem[7] ^ mem[MEMSZ - 1];
loadi:
    r[ip->a] = (uint64_t)(int64_t)ip->imm; NEXT();
mov:
    r[ip->a] = r[ip->b]; NEXT();
add:
    r[ip->a] = r[ip->b] + r[ip->c]; NEXT();
sub:
    r[ip->a] = r[ip->b] - r[ip->c]; NEXT();
mul:
    r[ip->a] = r[ip->b] * r[ip->c]; NEXT();
xor_:
    r[ip->a] = r[ip->b] ^ r[ip->c]; NEXT();
shr:
    r[ip->a] = r[ip->b] >> (ip->imm & 63); NEXT();
load:
    r[ip->a] = mem[r[ip->b] % MEMSZ]; NEXT();
store:
    mem[r[ip->a] % MEMSZ] = r[ip->b]; NEXT();
cmplt:
    r[ip->a] = r[ip->b] < r[ip->c]; NEXT();
jmpnz:
    if (r[ip->a]) { ip = code + ip->imm; goto *table[ip->op]; }
    NEXT();
jmp:
    ip = code + ip->imm;
    goto *table[ip->op];
}

int main(int argc, char **argv) {
    uint64_t n = argc > 1 ? (uint64_t)atol(argv[1]) : 3000000;
    /* h = mix(h, i); store; i-- ; loop while i */
    static struct insn code[] = {
        {OP_LOADI, 0, 0, 0, 1},          /*  0: h = 1              */
        {OP_LOADI, 2, 0, 0, 31},         /*  1: k = 31             */
        {OP_LOADI, 3, 0, 0, 1},          /*  2: one = 1            */
        {OP_MUL, 0, 0, 2, 0},            /*  3: h *= k             */
        {OP_XOR, 0, 0, 15, 0},           /*  4: h ^= i             */
        {OP_SHR, 4, 0, 0, 13},           /*  5: t = h >> 13        */
        {OP_XOR, 0, 0, 4, 0},            /*  6: h ^= t             */
        {OP_STORE, 0, 0, 0, 0},          /*  7: mem[h] = h         */
        {OP_LOAD, 5, 0, 0, 0},           /*  8: u = mem[h]         */
        {OP_ADD, 1, 1, 5, 0},            /*  9: acc += u           */
        {OP_SUB, 15, 15, 3, 0},          /* 10: i -= 1             */
        {OP_JMPNZ, 15, 0, 0, 3},         /* 11: if i goto 3        */
        {OP_HALT, 0, 0, 0, 0},
    };
    printf("%llu\n", (unsigned long long)run(code, n));
    return 0;
}
