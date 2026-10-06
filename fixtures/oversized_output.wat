;; abi-v1 module that claims to return far more output than the limit.
;; Host must reject during invocation ("output exceeds limit") without
;; touching unbacked memory.
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
    ;; Claims a 1 MiB output (1048576 bytes) at offset 0.
    (i64.or
      (i64.shl (i64.const 0) (i64.const 32))
      (i64.const 1048576))))
