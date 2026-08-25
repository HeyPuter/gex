;; The same ring with the registers passed as tail-call *arguments* instead of
;; through the context. Parameters are locals: no memory, and no instance
;; indirection either, which is what ruled out globals.
(module
  (import "gex" "memory" (memory 16))
  (func $blk (param $ctx i32)
        (param $r0 i64) (param $r1 i64) (param $r2 i64) (param $r3 i64)
        (param $r4 i64) (param $r5 i64) (param $r6 i64) (param $r7 i64)
    (if (i64.eqz (local.get $r0)) (then (return)))
    (local.set $r0 (i64.sub (local.get $r0) (i64.const 1)))
    (local.set $r1 (i64.add (local.get $r1) (local.get $r2)))
    (local.set $r3 (i64.xor (local.get $r3) (local.get $r1)))
    (return_call $blk (local.get $ctx)
      (local.get $r0) (local.get $r1) (local.get $r2) (local.get $r3)
      (local.get $r4) (local.get $r5) (local.get $r6) (local.get $r7)))
  (func (export "run") (param $n i32) (result i64)
    (call $blk (i32.const 0) (i64.extend_i32_u (local.get $n))
      (i64.const 1) (i64.const 3) (i64.const 7)
      (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))
    (i64.const 0))
)
