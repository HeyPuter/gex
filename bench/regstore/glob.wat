;; The same traffic through *imported mutable globals*. Imported because a
;; closure is many instances and a register has to be the same one in all of
;; them, which is how the memory is already shared.
(module
  (import "gex" "memory" (memory 16))
  (import "gex" "r0" (global $r0 (mut i64)))
  (import "gex" "r1" (global $r1 (mut i64)))
  (import "gex" "r2" (global $r2 (mut i64)))
  (import "gex" "r3" (global $r3 (mut i64)))
  (func (export "run") (param $n i32) (result i64)
    (local $i i32) (local $a i64) (local $b i64) (local $c i64) (local $d i64)
    (loop $L
      (global.set $r0 (local.get $a))
      (global.set $r1 (local.get $b))
      (global.set $r2 (local.get $c))
      (global.set $r3 (local.get $d))
      (local.set $a (i64.add (global.get $r0) (i64.const 1)))
      (local.set $b (i64.add (global.get $r1) (i64.const 3)))
      (local.set $c (i64.xor (global.get $r2) (local.get $a)))
      (local.set $d (i64.add (global.get $r3) (local.get $b)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $L (i32.lt_s (local.get $i) (local.get $n))))
    (i64.add (i64.add (local.get $a) (local.get $b))
             (i64.add (local.get $c) (local.get $d))))
)
