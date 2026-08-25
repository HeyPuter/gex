;; The same loop with a 64-bit memory and no guard at all: the engine's own
;; bounds check is the only one.
(module
  (memory (export "mem") i64 4096 4096)
  (func (export "run") (param $n i32) (result i64)
    (local $i i32) (local $a i64) (local $sum i64)
    (loop $L
      (local.set $a
        (i64.and (i64.mul (i64.extend_i32_u (local.get $i)) (i64.const 2654435761))
                 (i64.const 0x0fff_fff8)))
      (local.set $sum (i64.add (local.get $sum) (i64.load (local.get $a))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $L (i32.lt_s (local.get $i) (local.get $n))))
    (local.get $sum))
)
