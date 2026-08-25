;; The register traffic gex emits today: four values stored to the context on
;; the way out of a block and four loaded back on the way in.
(module
  (import "gex" "memory" (memory 16))
  (func (export "run") (param $n i32) (result i64)
    (local $i i32) (local $a i64) (local $b i64) (local $c i64) (local $d i64)
    (loop $L
      ;; flush
      (i64.store (i32.const 344) (local.get $a))
      (i64.store (i32.const 352) (local.get $b))
      (i64.store (i32.const 360) (local.get $c))
      (i64.store (i32.const 368) (local.get $d))
      ;; reload, as the next block would
      (local.set $a (i64.add (i64.load (i32.const 344)) (i64.const 1)))
      (local.set $b (i64.add (i64.load (i32.const 352)) (i64.const 3)))
      (local.set $c (i64.xor (i64.load (i32.const 360)) (local.get $a)))
      (local.set $d (i64.add (i64.load (i32.const 368)) (local.get $b)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $L (i32.lt_s (local.get $i) (local.get $n))))
    (i64.add (i64.add (local.get $a) (local.get $b))
             (i64.add (local.get $c) (local.get $d))))
)
