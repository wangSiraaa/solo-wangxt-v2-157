//! Acceptance tests at the sandbox boundary (no database required):
//!
//! * pure arithmetic module        -> success, correct output
//! * tight infinite loop           -> terminated (fuel), host survives
//! * over-privileged imports       -> rejected at instantiation, never runs
//! * memory bomb                   -> growth denied beyond the ceiling
//! * oversized output claim        -> rejected at invocation
//! * ordinary trap                 -> invocation stage, distinct from the above
//! * gauges return to zero after every case (resource reclamation)
//!
//! These are the exact scenarios the acceptance run exercises end-to-end.
use std::collections::BTreeSet;

use wasm_plugin_sandbox::limits::ResolvedLimits;
use wasm_plugin_sandbox::metrics::Metrics;
use wasm_plugin_sandbox::model::{ErrorStage, validate_and_encode, Contract};
use wasm_plugin_sandbox::runtime::SandboxEngine;

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(format!("{name}.wasm"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn limits() -> ResolvedLimits {
    ResolvedLimits {
        memory_bytes: 2 * 1024 * 1024, // 2 MiB ceiling
        fuel: 2_000_000,
        output_bytes: 4096,
        log_bytes: 4096,
        timeout_ms: 2_000,
    }
}

fn arith_contract() -> Contract {
    serde_json::from_value(serde_json::json!({
        "abi_version": "abi-v1",
        "inputs": [
            {"name": "a", "ty": "i64", "required": true},
            {"name": "b", "ty": "i64", "required": true}
        ],
        "output": {"format": "json"},
        "host_allowlist": ["host_log"]
    }))
    .unwrap()
}

fn stage_of(err: &wasm_plugin_sandbox::error::ApiError) -> ErrorStage {
    err.stage.expect("execution error must carry a stage")
}

#[test]
fn validation_rejects_garbage_and_missing_exports() {
    let m = Metrics::new();
    let engine = SandboxEngine::new(m).unwrap();

    // Random bytes never compile.
    let junk = vec![0x00u8, 0x61, 0x73, 0x6d, 0x99, 0x00, 0x00];
    let err = engine.validate(&junk).unwrap_err();
    assert!(err.message.contains("validation failed"), "got: {err}");

    // Valid wasm without the ABI exports is rejected too.
    let minimal = wat::parse_str("(module (func))").unwrap();
    let err = engine.validate(&minimal).unwrap_err();
    assert!(err.message.contains("missing required export"), "got: {err}");
}

#[test]
fn pure_arithmetic_succeeds_and_logs_are_captured() {
    let engine = SandboxEngine::new(Metrics::new()).unwrap();
    let wasm = fixture("arith");
    engine.validate(&wasm).unwrap(); // phase 1 ok

    let contract = arith_contract();
    let input = serde_json::json!({"a": 40, "b": 2});
    let args = validate_and_encode(&contract, &input, &Default::default()).unwrap();

    let out = engine
        .execute("h-arith", &wasm, &args, &contract.host_allowlist, limits())
        .expect("arithmetic task must succeed");

    assert_eq!(String::from_utf8(out.output).unwrap(), r#"{"sum":42}"#);
    assert_eq!(out.guest_logs, vec!["add ok".to_string()]);
    assert!(out.fuel_consumed > 0);

    // Negative sum path.
    let input = serde_json::json!({"a": -50, "b": 8});
    let args = validate_and_encode(&contract, &input, &Default::default()).unwrap();
    let out = engine
        .execute("h-arith", &wasm, &args, &contract.host_allowlist, limits())
        .unwrap();
    assert_eq!(String::from_utf8(out.output).unwrap(), r#"{"sum":-42}"#);
}

#[test]
fn infinite_loop_is_terminated_without_killing_the_host() {
    let engine = SandboxEngine::new(Metrics::new()).unwrap();
    let wasm = fixture("infinite_loop");
    engine.validate(&wasm).unwrap(); // well-formed, exports present

    let started = std::time::Instant::now();
    let err = engine
        .execute("h-loop", &wasm, &[], &BTreeSet::new(), limits())
        .expect_err("infinite loop must return an error");
    let elapsed = started.elapsed();

    assert_eq!(stage_of(&err), ErrorStage::Invocation);
    assert!(
        err.message.contains("fuel") || err.message.contains("deadline"),
        "expected termination reason, got: {err}"
    );
    // Fuel should end it far faster than the 2s epoch deadline; be generous.
    assert!(elapsed < std::time::Duration::from_secs(2), "took {elapsed:?}");

    // Host is still alive and can serve work immediately.
    let wasm = fixture("arith");
    let contract = arith_contract();
    let args = validate_and_encode(
        &contract,
        &serde_json::json!({"a": 1, "b": 1}),
        &Default::default(),
    )
    .unwrap();
    let out = engine
        .execute("h-arith2", &wasm, &args, &contract.host_allowlist, limits())
        .unwrap();
    assert_eq!(String::from_utf8(out.output).unwrap(), r#"{"sum":2}"#);
}

#[test]
fn unauthorized_imports_fail_at_instantiation_and_never_run() {
    let engine = SandboxEngine::new(Metrics::new()).unwrap();
    let wasm = fixture("unauthorized_imports");
    // Static validation passes: the module itself is well-formed.
    engine.validate(&wasm).unwrap();

    // Empty allowlist: sockets/files/exec/http cannot be linked.
    let err = engine
        .execute("h-bad-imports", &wasm, &[], &BTreeSet::new(), limits())
        .expect_err("linking must fail");
    assert_eq!(stage_of(&err), ErrorStage::Instantiation, "got: {err}");
    assert!(err.message.contains("allowlisted host"), "got: {err}");

    // Even if the contract had asked for host_log, the other imports remain.
    let mut allow = BTreeSet::new();
    allow.insert("host_log".to_string());
    let err = engine
        .execute("h-bad-imports2", &wasm, &[], &allow, limits())
        .expect_err("linking must still fail");
    assert_eq!(stage_of(&err), ErrorStage::Instantiation);
}

#[test]
fn memory_growth_beyond_ceiling_is_denied_and_task_ends() {
    let engine = SandboxEngine::new(Metrics::new()).unwrap();
    let wasm = fixture("memory_bomb");
    engine.validate(&wasm).unwrap();

    let err = engine
        .execute("h-mem", &wasm, &[], &BTreeSet::new(), limits())
        .expect_err("memory bomb cannot succeed");
    // The denied growth makes the guest retry forever; fuel ends it.
    assert_eq!(stage_of(&err), ErrorStage::Invocation);
    assert!(err.message.contains("fuel") || err.message.contains("deadline"), "got: {err}");
}

#[test]
fn oversized_output_is_rejected_at_invocation() {
    let engine = SandboxEngine::new(Metrics::new()).unwrap();
    let wasm = fixture("oversized_output");
    engine.validate(&wasm).unwrap();

    let err = engine
        .execute("h-out", &wasm, &[], &BTreeSet::new(), limits())
        .expect_err("oversized output must be rejected");
    assert_eq!(stage_of(&err), ErrorStage::Invocation);
    assert!(err.message.contains("output"), "got: {err}");
}

#[test]
fn ordinary_trap_is_reported_as_invocation_failure() {
    let engine = SandboxEngine::new(Metrics::new()).unwrap();
    let wasm = fixture("traps");
    engine.validate(&wasm).unwrap();

    let err = engine
        .execute("h-trap", &wasm, &[], &BTreeSet::new(), limits())
        .expect_err("unreachable must trap");
    assert_eq!(stage_of(&err), ErrorStage::Invocation);
    assert!(err.message.to_lowercase().contains("trap"), "got: {err}");
}

#[test]
fn resources_are_reclaimed_after_every_outcome() {
    let metrics = Metrics::new();
    let engine = SandboxEngine::new(metrics.clone()).unwrap();
    let allow: BTreeSet<String> = BTreeSet::new();

    // Success.
    let contract = arith_contract();
    let args = validate_and_encode(
        &contract,
        &serde_json::json!({"a": 1, "b": 2}),
        &Default::default(),
    )
    .unwrap();
    let _ = engine
        .execute("h1", &fixture("arith"), &args, &contract.host_allowlist, limits())
        .unwrap();
    assert_eq!(metrics.render().lines().find(|l| l.starts_with("sandbox_instances_live ")).unwrap(),
               "sandbox_instances_live 0");
    assert!(metrics.render().contains("sandbox_memory_bytes_live 0"));

    // Failure kinds.
    for f in ["infinite_loop", "memory_bomb", "traps", "oversized_output", "unauthorized_imports"] {
        let res = engine.execute(format!("h-{f}").as_str(), &fixture(f), &[], &allow, limits());
        assert!(res.is_err(), "{f} should fail");
        assert!(
            metrics.render().contains("sandbox_instances_live 0"),
            "leaked instance after {f}: {}",
            metrics.render()
        );
        assert!(
            metrics.render().contains("sandbox_memory_bytes_live 0"),
            "leaked memory accounting after {f}"
        );
    }
}

#[test]
fn input_contract_validation_runs_before_execution() {
    let contract = arith_contract();
    let g = Default::default();

    // Wrong type.
    let err = validate_and_encode(&contract, &serde_json::json!({"a": "x", "b": 2}), &g).unwrap_err();
    assert!(err.message.contains("parameter \"a\""), "got: {err}");

    // Missing required.
    let err = validate_and_encode(&contract, &serde_json::json!({"a": 1}), &g).unwrap_err();
    assert!(err.message.contains("missing required"), "got: {err}");

    // Not an object.
    let err = validate_and_encode(&contract, &serde_json::json!([1, 2]), &g).unwrap_err();
    assert!(err.message.contains("JSON object"), "got: {err}");
}

#[test]
fn whitelist_catalog_cannot_name_network_or_filesystem() {
    let g = Default::default();
    for bogus in ["http_get", "fs_read", "sock_open", "wasi_snapshot_preview1"] {
        let c: Contract = serde_json::from_value(serde_json::json!({
            "abi_version": "abi-v1",
            "inputs": [{"name": "x", "ty": "i64"}],
            "host_allowlist": [bogus]
        }))
        .unwrap();
        let err = wasm_plugin_sandbox::model::validate_contract(&c, 64, &g).unwrap_err();
        assert!(err.message.contains("not in the host whitelist"), "bogus={bogus} got: {err}");
    }
}
