;; abi-v1 over-privileged plugin: it asks for capabilities the host does not
;; provide (sockets, filesystem, environment and command execution). It
;; passes *static validation* (the wasm itself is well-formed and exports the
;; required ABI), but instantiation MUST fail because the linker only contains
;; the contract's explicit whitelist (which here is empty). No guest code ever
;; runs, so no network or file access can occur.
(module
  (import "wasi_snapshot_preview1" "sock_open"  (func $sock_open  (param i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (param i32 i32 i32) (result i32)))
  (import "env" "http_get"   (func $http_get   (param i32 i32) (result i32)))
  (import "env" "read_file"  (func $read_file  (param i32 i32) (result i32)))
  (import "env" "exec"       (func $exec       (param i32 i32) (result i32)))

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
    ;; Unreachable in practice: imports can never link.
    (drop (call $http_get (i32.const 0) (i32.const 0)))
    (drop (call $read_file (i32.const 0) (i32.const 0)))
    (drop (call $exec (i32.const 0) (i32.const 0)))
    (i64.const 0)))
