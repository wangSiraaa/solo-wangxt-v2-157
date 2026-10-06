# API Contract

Base URL: `http://<host>:8080`
All `/v1/*` endpoints require a tenant header:

| Header | Format |
|---|---|
| `x-tenant-id` | 1–64 chars, `[a-zA-Z0-9_-]` |

Requests and responses are JSON (`application/json`).

## Error envelope

```json
{ "error": { "code": "contract_input_violation", "message": "parameter \"a\": expected an integer" } }
```

Execution-phase errors carry an additional `stage` field:

```json
{ "error": { "code": "instantiation_failed", "stage": "instantiation",
             "message": "module imports cannot be satisfied by the allowlisted host: ..." } }
```

Stable error codes: `bad_request`, `body_too_large`, `not_found`, `conflict`,
`contract_input_violation`, `contract_invalid`, `module_invalid`,
`instantiation_failed`, `invocation_failed`, `unavailable`, `internal`.

The three execution phases are reported **separately**:

| Phase | When | code |
|---|---|---|
| `validation` | bytes fail to decode / type-check / miss required ABI exports | `module_invalid` (422 at upload) |
| `instantiation` | imports cannot be linked against the allowlist, instance setup fails | `instantiation_failed` |
| `invocation` | `run` traps, runs out of fuel, hits the wall-clock deadline, exceeds output limit, or returns output violating the contract | `invocation_failed` |

## Plugin ABI: `abi-v1`

A plugin module **must** export:

* `memory` — one linear memory, initial size ≥ 1 page (64 KiB)
* `run() -> i64` — entry point; returns `(offset << 32) | length` of its output
* `alloc(n: i32) -> i32` — guest bump allocator (required export; the host
  itself places inputs in a reserved top-of-page scratch region rather than
  trusting guest allocation)
* mutable globals `abi_args_ptr: i64`, `abi_args_len: i64` — set by the host
  to describe the argument block

Argument block binary layout:

```
[param_count: u32 LE]
repeated per parameter, in contract order:
  scalar (i64/u64/f64): [tag: u8][8 bytes LE]
  bool:                [tag: u8][7 zero bytes][0|1]
  string/bytes:        [tag: u8][len: u32 LE][bytes]   (bytes are base64-decoded)
```

Tags: `1=i64 2=u64 3=f64 4=bool 5=string 6=bytes`.
The argument block fits in the reserved 64 KiB ABI scratch area (enforced at
input validation, before any task row exists).

### Host-function allowlist

Only one import is defined by the host, and only linked when the contract
lists it:

| Module | Name | Signature | Meaning |
|---|---|---|---|
| `env` | `host_log` | `(ptr: i32, len: i32)` | append a UTF-8 log line to the task result; capped per task |

There is **no** network, filesystem, environment, clock or process
capability anywhere in the host. WASI is not linked. A module that imports
anything not provided passes static validation but fails **instantiation**
before a single guest instruction runs.

## Endpoints

### `POST /v1/plugins` — validate and register a plugin

```json
{
  "name": "arith",
  "contract": {
    "abi_version": "abi-v1",
    "inputs": [
      { "name": "a", "ty": "i64", "required": true },
      { "name": "b", "ty": "i64", "required": true }
    ],
    "output": { "format": "json" },
    "host_allowlist": ["host_log"],
    "limits": { "memory_mib": 4, "fuel": 2000000, "output_kib": 16, "timeout_ms": 2000 }
  },
  "wasm_base64": "AGFzbQ..."
}
```

`201`:

```json
{ "plugin_id": "plg_...", "sha256": "...", "interface": { "imports": [...], "exports": [...] } }
```

Validation order (both must pass before anything is stored):
contract-by-value → static wasm validation (decode + type check + required
exports). `limits` in the contract can only **reduce** the server ceilings.

### `GET /v1/plugins`, `GET /v1/plugins/{id}`

Tenant-scoped summaries and contracts. Wasm bytes are never returned.

### `POST /v1/plugins/{id}/invoke` — run a task (synchronous)

Body: the input object, validated against the contract.

* Bad input → `422 contract_input_violation`, **no task is created**.
* Accepted → always `201` with the terminal task (execution is synchronous):

```json
{
  "task_id": "tsk_...", "tenant": "tenant-a", "plugin_id": "plg_...",
  "status": "succeeded",
  "output": { "sum": 42 },
  "guest_logs": ["add ok"],
  "fuel_consumed": 1234, "duration_ms": 1,
  "created_at": "...", "finished_at": "..."
}
```

Failure:

```json
{
  "task_id": "tsk_...", "status": "failed",
  "error_stage": "invocation", "error_code": "invocation_failed",
  "error_message": "task terminated: fuel budget exhausted after 10000000 fuel (possible infinite loop)",
  "output": null,
  "fuel_consumed": 10000000, "duration_ms": 9,
  "created_at": "...", "finished_at": "...", "guest_logs": []
}
```

`output` is present **iff** `status == "succeeded"`; it is `null` for every
failure. The database enforces this with CHECK constraints, so a failed task
can never carry a partial result.

### `GET /v1/tasks`, `GET /v1/tasks/{id}`

Tenant-scoped task history and lookup.

### Ops

* `GET /healthz` — `{"status":"ok"}` or 503 when Postgres is unreachable.
* `GET /metrics` — Prometheus text. Key gauges for proving reclamation:
  `sandbox_instances_live`, `sandbox_memory_bytes_live`,
  `sandbox_calls_in_flight` (all 0 when idle); counters include
  `sandbox_tasks_fuel_exhausted_total`, `sandbox_tasks_timed_out_total`,
  `sandbox_memory_growth_denied_total`, `sandbox_output_oversized_total`.

## Resource limits (server defaults; override via env)

| Resource | Env | Default |
|---|---|---|
| linear memory / task | `SANDBOX_MAX_MEMORY_MIB` | 16 MiB |
| fuel / task (≈ instructions) | `SANDBOX_MAX_FUEL` | 10,000,000 |
| output / task | `SANDBOX_MAX_OUTPUT_KB` | 64 KiB |
| guest host-log / task | — | 16 KiB |
| wall clock / task | `SANDBOX_TIMEOUT_MS` | 10,000 ms |
| upload size | — | 8 MiB |
| concurrent guest tasks | `MAX_CONCURRENT_EXECS` | 64 |

Two independent termination mechanisms stop an infinite loop: **fuel**
(deterministic, normally fires first) and the **epoch deadline** (wall-clock
backstop from a 20 ms ticker thread), enforced inside Wasmtime regardless of
where the guest is executing.
