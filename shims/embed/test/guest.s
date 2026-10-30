// The harness's guest code (test/embedtest.c): AArch64 functions, assembled by build.sh into a blob the test copies
// into memory it allocated and marks executable.
.text

// x0 * x1, by repeated addition: a loop (back edges, safepoints).
.globl mul_loop
mul_loop:
    mov x2, #0
1:  add x2, x2, x0
    subs x1, x1, #1
    b.ne 1b
    mov x0, x2
    ret

// x2(x0, x1) + 1: a call through a register to a host function, with a frame.
.globl call_host
call_host:
    stp x29, x30, [sp, #-16]!
    mov x29, sp
    blr x2
    add x0, x0, #1
    ldp x29, x30, [sp], #16
    ret

// write(1, x0, x1): a system call.
.globl say
say:
    mov x2, x1
    mov x1, x0
    mov x0, #1
    mov x8, #64
    svc #0
    ret

// clock_gettime(CLOCK_MONOTONIC, x0).
.globl clock
clock:
    mov x1, x0
    mov x0, #1
    mov x8, #113
    svc #0
    ret

// d0 + d1.
.globl fadd2
fadd2:
    fadd d0, d0, d1
    ret

// s0 * v1.s[2]: a scalar by-element multiply.
.globl fmul_lane
fmul_lane:
    fmul s0, s0, v1.s[2]
    ret

// d0 + d1 * v2.d[1].
.globl fmla_lane
fmla_lane:
    fmla d0, d1, v2.d[1]
    ret

// x0 + the ninth and tenth arguments, which are on the stack.
.globl stack_args
stack_args:
    ldr x9, [sp]
    ldr x10, [sp, #8]
    add x0, x0, x9
    add x0, x0, x10
    ret

// Add 1 to *x0, x1 times, with an exclusive pair.
.globl inc_excl
inc_excl:
1:  ldxr x2, [x0]
    add x2, x2, #1
    stxr w3, x2, [x0]
    cbnz w3, 1b
    subs x1, x1, #1
    b.ne 1b
    ret

// Add 1 to *x0, x1 times, with LSE's ldaddal.
.globl inc_lse
inc_lse:
    mov x2, #1
1:  ldaddal x2, x3, [x0]
    subs x1, x1, #1
    b.ne 1b
    ret
