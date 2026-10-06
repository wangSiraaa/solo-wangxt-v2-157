//! Compiles fixtures/*.wat into fixtures/*.wasm once.
//! Run with: cargo test --test build_fixtures
use std::path::Path;

#[test]
fn build_wat_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    for stem in [
        "arith",
        "infinite_loop",
        "unauthorized_imports",
        "memory_bomb",
        "traps",
        "oversized_output",
    ] {
        let wat_path = dir.join(format!("{stem}.wat"));
        let wasm_path = dir.join(format!("{stem}.wasm"));
        let bytes = wat::parse_file(&wat_path)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", wat_path.display()));
        std::fs::write(&wasm_path, &bytes)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", wasm_path.display()));
        eprintln!("{} -> {} ({} bytes)", wat_path.display(), wasm_path.display(), bytes.len());
    }
}
