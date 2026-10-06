;; Pure arithmetic plugin: sums the raw input bytes and returns {"sum":N}.
;; Demonstrates the happy path plus the whitelisted host_log import.
(module
  (import "env" "host_log" (func $host_log (param i32 i32 i32)))

  (memory (export "memory") 4)
  (global $heap (mut i32) (i32.const 4096))

  (data (i32.const 512) "arith: run started")

  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))

  (func (export "run") (param $ptr i32) (param $len i32) (result i64)
    (local $i i32)
    (local $sum i32)
    (local $out i32)
    (local $p i32)
    (local $n i32)
    (local $digits i32)
    (local $scratch i32)

    ;; whitelisted host call: level 1 = info
    (call $host_log (i32.const 1) (i32.const 512) (i32.const 18))

    ;; sum the raw input bytes
    (block $sum_done
      (loop $sum_loop
        (br_if $sum_done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $sum
          (i32.add (local.get $sum)
            (i32.load8_u (i32.add (local.get $ptr) (local.get $i)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $sum_loop)))

    ;; output buffer + scratch area for digit reversal
    (local.set $out (global.get $heap))
    (local.set $scratch (i32.add (local.get $out) (i32.const 64)))

    ;; write "{\"sum\":" (7 bytes; the 8th is overwritten by digits below)
    (i32.store (local.get $out) (i32.const 0x7573227b))                ;; { " s u
    (i32.store (i32.add (local.get $out) (i32.const 4)) (i32.const 0x203a226d)) ;; m " : _
    (local.set $p (i32.add (local.get $out) (i32.const 7)))

    (local.set $n (local.get $sum))
    (if (i32.eqz (local.get $n))
      (then
        (i32.store8 (local.get $p) (i32.const 48)) ;; '0'
        (local.set $p (i32.add (local.get $p) (i32.const 1))))
      (else
        ;; digits, reversed, into scratch
        (block $digits_done
          (loop $digits_loop
            (br_if $digits_done (i32.eqz (local.get $n)))
            (i32.store8
              (i32.add (local.get $scratch) (local.get $digits))
              (i32.add (i32.const 48) (i32.rem_u (local.get $n) (i32.const 10))))
            (local.set $n (i32.div_u (local.get $n) (i32.const 10)))
            (local.set $digits (i32.add (local.get $digits) (i32.const 1)))
            (br $digits_loop)))
        ;; copy them back in order
        (local.set $i (i32.const 0))
        (block $copy_done
          (loop $copy_loop
            (br_if $copy_done (i32.ge_u (local.get $i) (local.get $digits)))
            (i32.store8
              (i32.add (local.get $p) (local.get $i))
              (i32.load8_u
                (i32.add (local.get $scratch)
                  (i32.sub (i32.sub (local.get $digits) (i32.const 1)) (local.get $i)))))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_loop)))
        (local.set $p (i32.add (local.get $p) (local.get $digits)))))

    ;; closing brace
    (i32.store8 (local.get $p) (i32.const 125)) ;; }
    (local.set $p (i32.add (local.get $p) (i32.const 1)))

    ;; return (out_ptr << 32) | out_len
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (i32.sub (local.get $p) (local.get $out))))))
