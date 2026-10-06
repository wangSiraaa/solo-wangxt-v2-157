;; Memory hog: grows linear memory one page at a time until the resource
;; limiter refuses, then exits cleanly. The host must stay healthy and the
;; task itself must succeed (growth refusal is not a trap).
(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))

  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))

  (func (export "run") (param i32 i32) (result i64)
    (local $out i32)
    ;; grow until the limiter says no (memory.grow returns -1)
    (block $done
      (loop $grow
        (br_if $done (i32.lt_s (memory.grow (i32.const 1)) (i32.const 0)))
        (br $grow)))

    ;; write a fixed "{\"ok\":true}" (11 bytes) at the heap pointer
    (local.set $out (global.get $heap))
    (i32.store (local.get $out) (i32.const 0x6b6f227b))                          ;; { " o k
    (i32.store (i32.add (local.get $out) (i32.const 4)) (i32.const 0x72743a22)) ;; " : t r
    (i32.store16 (i32.add (local.get $out) (i32.const 8)) (i32.const 0x6575))   ;; u e
    (i32.store8 (i32.add (local.get $out) (i32.const 10)) (i32.const 125))      ;; }

    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.const 11))))
