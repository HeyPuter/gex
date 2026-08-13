/* Flags living across basic-block boundaries: every case below reads a flag in
 * a different block from the one that set it, which cross-block flag-liveness
 * analysis must not optimise away. Each case appends one character to a buffer
 * written at the end, so a wrong branch shows up as a wrong string. */
    .text
    .global _start

/* x20 = output cursor. `emit` appends w0. */
emit:
    strb    w0, [x20], #1
    ret

_start:
    adrp    x20, buf
    add     x20, x20, :lo12:buf
    mov     x21, x20

/* 1. Unconditional branch between the compare and its use. */
    mov     x0, #7
    mov     x1, #7
    cmp     x0, x1
    b       1f
1:  b.eq    2f
    mov     w0, #'x'
    bl      emit
    b       3f
2:  mov     w0, #'a'
    bl      emit
3:

/* 2. The same, taking the other side: C and V, not just Z. */
    mov     x0, #-5
    mov     x1, #3
    cmp     x0, x1
    b       4f
4:  b.lt    5f
    mov     w0, #'x'
    bl      emit
    b       6f
5:  mov     w0, #'b'
    bl      emit
6:

/* 3. A flag set in one block, read in a block two edges away, with an
      unrelated (non-flag-setting) instruction in between. */
    mov     x0, #100
    mov     x1, #100
    subs    x2, x0, x1
    b       7f
7:  add     x3, x3, #1
    b       8f
8:  b.ne    9f
    mov     w0, #'c'
    bl      emit
    b       10f
9:  mov     w0, #'x'
    bl      emit
10:

/* 4. Flags live across a call, which is legal whenever the callee preserves
      them. `emit` does not touch NZCV. */
    mov     x0, #4
    mov     x1, #9
    cmp     x0, x1
    mov     w0, #'d'
    bl      emit
    b.lo    11f
    mov     w0, #'x'
    bl      emit
    b       12f
11: mov     w0, #'e'
    bl      emit
12:

/* 5. Carry specifically, consumed by adc in a later block. */
    mov     x0, #-1
    adds    x1, x0, #1          /* sets C */
    b       13f
13: mov     x2, #0
    adc     x2, x2, xzr         /* x2 = 1 if C */
    cmp     x2, #1
    b.eq    14f
    mov     w0, #'x'
    bl      emit
    b       15f
14: mov     w0, #'f'
    bl      emit
15:

/* 6. Overflow specifically, read after a branch. */
    mov     x0, #0x7fffffffffffffff
    adds    x1, x0, #1          /* sets V */
    b       16f
16: b.vs    17f
    mov     w0, #'x'
    bl      emit
    b       18f
17: mov     w0, #'g'
    bl      emit
18:

/* 7. A loop whose exit test is in a different block from its compare. */
    mov     x9, #0
    mov     x10, #5
19: add     x9, x9, #1
    cmp     x9, x10
    b       20f
20: b.ne    19b
    mov     w0, #'h'
    bl      emit

/* 8. ccmp, which reads flags to decide whether to write them. */
    mov     x0, #3
    mov     x1, #3
    cmp     x0, x1              /* Z = 1 */
    b       21f
21: ccmp    x0, #4, #0, eq      /* eq holds, so compare: 3 != 4 -> Z = 0 */
    b       22f
22: b.ne    23f
    mov     w0, #'x'
    bl      emit
    b       24f
23: mov     w0, #'i'
    bl      emit
24:

    mov     w0, #'\n'
    bl      emit

/* write(1, buf, cursor - start) */
    sub     x2, x20, x21
    mov     x1, x21
    mov     x0, #1
    mov     x8, #64
    svc     #0
    mov     x0, #0
    mov     x8, #93
    svc     #0

    .bss
    .align  3
buf:
    .space  64
