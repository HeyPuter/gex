/* co_call(cb, arg): cb(arg) + 1, the call's return address the entry of
   co_after, a function of its own: the call-out's return is an entry of the
   unit, which dispatch reaches through its bridge. */
	.text
	.globl	co_call
	.type	co_call, %function
	.globl	co_after
	.type	co_after, %function
	.p2align 2
co_call:
	stp	x29, x30, [sp, #-16]!
	mov	x29, sp
	mov	x2, x0
	mov	x0, x1
	blr	x2
co_after:
	add	x0, x0, #1
	ldp	x29, x30, [sp], #16
	ret
	.size	co_call, co_after-co_call
	.size	co_after, .-co_after
	.section .note.GNU-stack,"",%progbits
