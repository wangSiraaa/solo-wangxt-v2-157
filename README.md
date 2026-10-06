# wasm-plugin-sandbox

Third-party compute plugin platform — a server-side HTTP API (Rust +
[Axum](https://github.com/tokio-rs/axum) +
[Wasmtime](https://wasmtime.dev/) + PostgreSQL) that runs untrusted
WebAssembly modules inside an explicit, deniable resource boundary. No
frontend.

## What it guarantees

* **Explicit host-function whitelist.** The linker starts empty for every
  task and is populated only with functions named in the plugin contract.
  The catalog contains exactly one capability (`env.host_log`). There is no
  WASI, no socket, no filesystem, no environment, no clock. A module that
  imports anything else fails *instantiation* and never executes.
* **No network / no arbitrary file access by default.** Such capability
  names do not exist in the host, so they cannot be granted accidentally;
  naming one in a contract is rejected at upload.
* **Per-task limits on memory, fuel, output and log volume.** Contract
  requests are clamped against hard server ceilings (a tenant can only ask
  for *less*).
* **Infinite loops cannot take down the host.** Fuel gives a deterministic
  instruction budget; an epoch deadline on a dedicated ticker thread gives a
  wall-clock backstop. Guest execution happens on Tokio blocking threads
  behind a host-wide concurrency semaphore.
* **Three distinct failure phases.** Static module validation, instance
  instantiation and the guest call each produce their own stage/code, so
  callers can distinguish "bad artifact" from "missing capability" from
  "task was killed for exceeding its budget".
* **Contract-checked inputs before execution.** Type/range/presence
  violations return `422` and never create a task row.
* **No half-success.** Output is written only in the same transactional
  UPDATE that flips a task to `succeeded`; failure leaves `output = NULL`.
  CHECK constraints enforce it in the database.
* **Deterministic resource reclamation.** Dropping the Wasmtime `Store`
  frees the instance and its linear memory on every return path; a `Drop`
  hook pairs the live gauges, observable at `/metrics`
  (`sandbox_instances_live`, `sandbox_memory_bytes_live`,
  `sandbox_calls_in_flight` return to 0).
* **Tenant isolation in logs.** Logs are structured JSON carrying only task
  ids, plugin ids, sizes and outcome codes — never input values. Tenant
  scoping is applied on every query, cross-tenant reads return 404.

## Layout

```
src/
  limits.rs    hard ceilings + per-contract clamped ResolvedLimits
  model.rs     contract types, ABI-v1 argument encoding, task/plugin records
  runtime.rs   Wasmtime engine: validate / instantiate / run, fuel, epoch, limiter
  service.rs   orchestration: upload pipeline, task lifecycle, bounded execution
  db.rs        PostgreSQL: migrations, tenant-scoped queries, atomic finalize
  http.rs      Axum router, DTOs, tenant header
  error.rs     typed errors / stable codes / phases
  metrics.rs   Prometheus text metrics
  config.rs    env configuration
fixtures/      .wat source + compiled .wasm for the acceptance modules
tests/         build_fixtures, sandbox_acceptance (10), epoch_backstop
scripts/acceptance_e2e.py   full black-box E2E suite (40 checks)
docs/API.md    HTTP + ABI contract
migrations/    embedded SQL applied at startup
```

## Quick start

```bash
# stack
docker compose up --build

# or, locally
cp .env.example .env
cargo run --release
# requires DATABASE_URL; migrations run automatically at startup
```

### Run the acceptance suites

```bash
# 1) boundary-level unit/integration tests (no database needed)
cargo test

# 2) black-box end-to-end against a running server + Postgres
scripts/acceptance_e2e.py --base-url http://127.0.0.1:8080 --psql "$(command -v psql)"
```

The bundled fixtures are the exact adversarial workloads:

| Fixture | Behavior | Expected result |
|---|---|---|
| `arith.wasm` | pure arithmetic, uses `host_log` | `succeeded`, `{"sum":N}` |
| `infinite_loop.wat` | `(loop (br 0))` | **`failed/invocation`**, terminated by fuel or deadline |
| `unauthorized_imports.wat` | imports sockets/files/exec/http | **`failed/instantiation`**, guest never runs |
| `memory_bomb.wat` | unbounded `memory.grow` | growth denied, task ends, host RSS unaffected |
| `oversized_output.wat` | claims 1 MiB output | **`failed/invocation`** (output limit) |
| `traps.wat` | `unreachable` in `run` | **`failed/invocation`** (ordinary trap) |

The E2E suite additionally verifies that normal tasks keep succeeding *after*
the adversarial ones, that all live gauges return to zero, that tenants
cannot read each other's resources, and (directly in Postgres) that no
failed row has output and no succeeded row lacks it.

## Configuration

See `.env.example`. All ceilings are environment-overridable; per-plugin
contract limits are clamped downwards against them.
