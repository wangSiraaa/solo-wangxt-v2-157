//! Wall-clock backstop: with a fuel budget that would take far too long to
//! burn, epoch interruption must still terminate the guest near the
//! configured timeout. Uses the infinite-loop fixture and a 300ms deadline.
use std::collections::BTreeSet;

use wasm_plugin_sandbox::limits::ResolvedLimits;
use wasm_plugin_sandbox::metrics::Metrics;
use wasm_plugin_sandbox::model::ErrorStage;
use wasm_plugin_sandbox::runtime::SandboxEngine;

#[test]
fn epoch_deadline_terminates_even_when_fuel_is_huge() {
    let engine = SandboxEngine::new(Metrics::new()).unwrap();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("infinite_loop.wasm");
    let wasm = std::fs::read(path).unwrap();

    let limits = ResolvedLimits {
        memory_bytes: 1024 * 1024,
        fuel: u64::MAX / 2, // effectively unreachable by fuel in the test window
        output_bytes: 4096,
        log_bytes: 4096,
        timeout_ms: 300,
    };

    let started = std::time::Instant::now();
    let err = engine
        .execute("h-epoch", &wasm, &[], &BTreeSet::new(), limits)
        .expect_err("must be terminated");
    let elapsed = started.elapsed();

    assert_eq!(err.stage, Some(ErrorStage::Invocation));
    assert!(
        err.message.contains("deadline") || err.message.contains("fuel"),
        "got: {err}"
    );
    // Must be close to the 300ms deadline (allow scheduling slack), never
    // runaway, and never over a couple of seconds.
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "epoch backstop too slow: {elapsed:?}"
    );
}
