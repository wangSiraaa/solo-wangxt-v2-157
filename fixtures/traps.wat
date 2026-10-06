;; abi-v1 well-behaved module that traps during the call (unreachable).
;; Validation and instantiation both succeed; only invocation fails.
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
    unreachable))
