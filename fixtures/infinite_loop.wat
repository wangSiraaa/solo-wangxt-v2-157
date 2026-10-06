;; abi-v1 malicious plugin: a tight infinite loop. It has no imports and
;; otherwise conforms to abi-v1, so it passes validation; the host must
;; terminate it by fuel/epoch deadline and reclaim its instance.
(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (global (export "abi_args_ptr") (mut i64) (i64.const 0))
  (global (export "abi_args_len") (mut i64) (i64.const 0))

  (func $alloc (export "alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (local.get $p) (local.get $n)))
    (local.get $p))

  (func $run (export "run") (result i64)
    (loop $forever
      (br $forever))
    (i64.const 0)))
