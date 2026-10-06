;; Traps immediately with `unreachable`. Upload and instantiation succeed;
;; the failure must surface as `invocation_failed`.
(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))

  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))

  (func (export "run") (param i32 i32) (result i64)
    unreachable))
