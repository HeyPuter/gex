;; memory32 with gex's explicit bounds guard: the address arrives as i64 and
;; every access tests it against the guest limit before wrapping to i32.
(module
  (memory (export "mem") 4096 4096)
  (func (export "run") (param $n i32) (result i64)
    (local $i i32) (local $a i64) (local $t i64) (local $sum i64)
    (loop $L
      (local.set $a
        (i64.and (i64.mul (i64.extend_i32_u (local.get $i)) (i64.const 2654435761))
                 (i64.const 0x0fff_fff8)))
      (local.set $sum (i64.add (local.get $sum)
        (i64.load
          (if (result i32)
            (i64.lt_u (local.tee $t (local.get $a)) (i64.const 0x1000_0000))
            (then (i32.wrap_i64 (local.get $t)))
            (else (i32.const 0x0fff_fff0))))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $L (i32.lt_s (local.get $i) (local.get $n))))
    (local.get $sum))
)
