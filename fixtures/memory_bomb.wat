;; abi-v1 memory-abuse plugin: tries to grow linear memory without bound.
;; The host's ResourceLimiter denies growth past the contract ceiling, and
;; memory.grow returns -1 here; the guest loops trying again (fuel then ends
;; it). Either way the host's RSS never approaches the guest's demand.
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
    (local $pages i32) (local $prev i32)
    ;; Start from a huge first request: 2 GiB in pages.
    (local.set $pages (i32.const 32768))
    (loop $grow
      (local.set $prev (memory.grow (local.get $pages)))
      ;; -1 on denial; keep hammering. Fuel ensures termination.
      (br $grow))
    (i64.const 0)))
