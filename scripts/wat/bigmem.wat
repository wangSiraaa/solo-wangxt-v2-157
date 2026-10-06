;; Big-memory module: declares 2000 initial pages (~125 MiB), above the sandbox
;; memory limit. The module itself is valid wasm, so upload succeeds, but
;; instantiation must fail with `instantiation_failed`.
(module
  (memory (export "memory") 2000)
  (global $heap (mut i32) (i32.const 1024))

  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))

  (func (export "run") (param i32 i32) (result i64)
    (i64.const 0)))
