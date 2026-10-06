;; abi-v1 arithmetic plugin: inputs (a: i64, b: i64) -> {"sum": <a+b>}
;; Uses the whitelisted env.host_log. No other imports exist.
(module
  (import "env" "host_log" (func $host_log (param i32 i32)))

  (memory (export "memory") 1)

  ;; Bump heap; host and guest share this allocator. Starts well clear of
  ;; the static data segment at 2048.
  (global $heap (mut i32) (i32.const 4096))
  (global (export "abi_args_ptr") (mut i64) (i64.const 0))
  (global (export "abi_args_len") (mut i64) (i64.const 0))

  (data (i32.const 2048) "add ok")

  (func $alloc (export "alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (local.get $p) (local.get $n)))
    (local.get $p))

  (func $put (param $addr i32) (param $byte i32)
    (i32.store8 (local.get $addr) (local.get $byte)))

  ;; Decimal length of an unsigned i64.
  (func $u64len (param $n i64) (result i32)
    (local $i i32) (local $x i64)
    (local.set $i (i32.const 1))
    (local.set $x (local.get $n))
    (block $done
      (loop $l
        (local.set $x (i64.div_u (local.get $x) (i64.const 10)))
        (br_if $done (i64.eqz (local.get $x)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (local.get $i))

  ;; Write unsigned i64 $n as decimal at $p; returns digit count.
  (func $u64toa (param $p i32) (param $n i64) (result i32)
    (local $len i32) (local $i i32) (local $x i64) (local $d i32)
    (local.set $len (call $u64len (local.get $n)))
    (local.set $i (local.get $len))
    (local.set $x (local.get $n))
    (block $done
      (loop $l
        (local.set $d (i32.wrap_i64 (i64.rem_u (local.get $x) (i64.const 10))))
        (call $put
          (i32.add (local.get $p) (i32.sub (local.get $i) (i32.const 1)))
          (i32.add (local.get $d) (i32.const 48)))
        (local.set $x (i64.div_u (local.get $x) (i64.const 10)))
        (local.set $i (i32.sub (local.get $i) (i32.const 1)))
        (br_if $done (i32.eqz (local.get $i)))
        (br $l)))
    (local.get $len))

  (func $run (export "run") (result i64)
    (local $ap i32) (local $a i64) (local $b i64) (local $s i64)
    (local $u i64) (local $neg i32) (local $p i32)
    (local $diglen i32) (local $outlen i32) (local $cursor i32)

    ;; Argument block layout for (i64, i64):
    ;;   [0..4)  count u32
    ;;   [4]     tag0   [5..13)  a i64 LE
    ;;   [13]    tag1   [14..22) b i64 LE
    (local.set $ap (i32.wrap_i64 (global.get 1)))
    (local.set $a (i64.load (i32.add (local.get $ap) (i32.const 5))))
    (local.set $b (i64.load (i32.add (local.get $ap) (i32.const 14))))
    (local.set $s (i64.add (local.get $a) (local.get $b)))

    (local.set $neg (i32.const 0))
    (if (i64.lt_s (local.get $s) (i64.const 0))
      (then (local.set $neg (i32.const 1))))
    ;; Unsigned absolute value (works for i64::MIN too).
    (local.set $u
      (select
        (i64.sub (i64.const 0) (local.get $s))
        (local.get $s)
        (local.get $neg)))

    ;; Output: {"sum":-?dddd}  -> reserve 32 bytes.
    (local.set $p (call $alloc (i32.const 32)))
    (call $put (i32.add (local.get $p) (i32.const 0)) (i32.const 123))  ;; {
    (call $put (i32.add (local.get $p) (i32.const 1)) (i32.const 34))   ;; "
    (call $put (i32.add (local.get $p) (i32.const 2)) (i32.const 115))  ;; s
    (call $put (i32.add (local.get $p) (i32.const 3)) (i32.const 117))  ;; u
    (call $put (i32.add (local.get $p) (i32.const 4)) (i32.const 109))  ;; m
    (call $put (i32.add (local.get $p) (i32.const 5)) (i32.const 34))   ;; "
    (call $put (i32.add (local.get $p) (i32.const 6)) (i32.const 58))   ;; :

    (if (i32.eqz (local.get $neg))
      (then)
      (else (call $put (i32.add (local.get $p) (i32.const 7)) (i32.const 45)))) ;; -

    (local.set $cursor (i32.add (local.get $p) (i32.add (i32.const 7) (local.get $neg))))
    (local.set $diglen (call $u64toa (local.get $cursor) (local.get $u)))
    (local.set $outlen (i32.add (i32.add (i32.const 8) (local.get $neg)) (local.get $diglen)))
    ;; closing brace at p + 7 + neg + diglen
    (call $put
      (i32.add (local.get $cursor) (local.get $diglen))
      (i32.const 125)) ;; }

    (call $host_log (i32.const 2048) (i32.const 6))

    ;; Packed return: (offset << 32) | length.
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
      (i64.extend_i32_u (local.get $outlen))))
)
