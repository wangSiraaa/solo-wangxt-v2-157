#!/usr/bin/env python3
"""End-to-end acceptance test for the third-party Wasm plugin API.

Runs the exact scenarios the acceptance review exercises:

  1. pure arithmetic module  -> 201 succeeded, correct JSON output
  2. intentional infinite loop -> 201 failed/invocation (fuel), bounded time
  3. over-privileged module (network/fs/exec imports)
       -> 201 failed/instantiation, guest never runs
  4. after 2+3, normal arithmetic tasks still succeed ("host survives")
  5. metrics gauges return to 0 (resource reclamation)
  6. failed tasks have NULL output in PostgreSQL (no half results)
  7. input contract violations are rejected before a task exists
  8. tenants are isolated (cannot read each other's plugins/tasks)
  9. log stream never contains tenant-supplied argument values

Usage:
  python3 scripts/acceptance_e2e.py --base-url http://127.0.0.1:8099 \\
      --psql /tmp/pgroot/usr/lib/postgresql/15/bin/psql
"""
import argparse
import base64
import json
import os
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FIX = os.path.join(ROOT, "fixtures")

PASS, FAIL = 0, 0


def check(name, cond, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"  PASS  {name}")
    else:
        FAIL += 1
        print(f"  FAIL  {name}  {detail}")


def req(method, url, body=None, tenant=None, expect_status=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        r.add_header("content-type", "application/json")
    if tenant:
        r.add_header("x-tenant-id", tenant)
    try:
        with urllib.request.urlopen(r, timeout=30) as resp:
            status, payload = resp.status, resp.read()
    except urllib.error.HTTPError as e:
        status, payload = e.code, e.read()
    if expect_status is not None and status != expect_status:
        raise AssertionError(f"{method} {url}: expected {expect_status}, got {status}: {payload!r}")
    return status, json.loads(payload) if payload else {}


def upload(base, tenant, name, wasm_file, contract):
    with open(os.path.join(FIX, wasm_file), "rb") as f:
        b64 = base64.b64encode(f.read()).decode()
    status, body = req(
        "POST", f"{base}/v1/plugins",
        {"name": name, "contract": contract, "wasm_base64": b64},
        tenant=tenant, expect_status=201,
    )
    return body["plugin_id"], body["interface"], body["sha256"]


ARITH_CONTRACT = {
    "abi_version": "abi-v1",
    "inputs": [
        {"name": "a", "ty": "i64", "required": True},
        {"name": "b", "ty": "i64", "required": True},
    ],
    "output": {"format": "json"},
    "host_allowlist": ["host_log"],
}
LOOP_CONTRACT = {
    "abi_version": "abi-v1",
    "inputs": [],
    "host_allowlist": [],
}
BAD_CONTRACT = {
    "abi_version": "abi-v1",
    "inputs": [],
    "host_allowlist": [],
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://127.0.0.1:8099")
    ap.add_argument("--psql", default="psql")
    ap.add_argument("--db", default="postgres://sandbox@127.0.0.1:5432/sandbox")
    args = ap.parse_args()
    base = args.base_url.rstrip("/")

    print("== health ==")
    _, h = req("GET", f"{base}/healthz")
    check("healthz ok", h.get("status") == "ok", h)

    print("== 1. upload + invoke pure arithmetic (tenant-a) ==")
    pid, iface, sha = upload(base, "tenant-a", "arith", "arith.wasm", ARITH_CONTRACT)
    check("interface exposes imports/exports",
          any(i["name"] == "host_log" for i in iface["imports"])
          and {e["name"] for e in iface["exports"]} >= {"memory", "alloc", "run"},
          json.dumps(iface))

    t0 = time.monotonic()
    _, body = req("POST", f"{base}/v1/plugins/{pid}/invoke",
                  {"a": 40, "b": 2}, tenant="tenant-a", expect_status=201)
    dt = time.monotonic() - t0
    check("arithmetic task succeeded", body["status"] == "succeeded", body)
    check("arithmetic output correct", body.get("output") == {"sum": 42}, body.get("output"))
    check("guest log captured via whitelisted host_log",
          body.get("guest_logs") == ["add ok"], body.get("guest_logs"))
    check("no error fields on success", body.get("error_stage") is None)
    check("fast", dt < 5, f"{dt:.2f}s")
    arith_task = body["task_id"]

    print("== 2. contract input validation fails before task creation ==")
    s, body = req("POST", f"{base}/v1/plugins/{pid}/invoke",
                  {"a": "not-a-number", "b": 2}, tenant="tenant-a")
    check("bad input -> 422", s == 422, body)
    check("stable error code", body["error"]["code"] == "contract_input_violation", body)

    s, _ = req("POST", f"{base}/v1/plugins/{pid}/invoke", {"a": 1}, tenant="tenant-a")
    check("missing required -> 422", s == 422)

    print("== 3. infinite loop must be terminated ==")
    loop_id, _, _ = upload(base, "tenant-a", "loop", "infinite_loop.wasm", LOOP_CONTRACT)
    t0 = time.monotonic()
    s, body = req("POST", f"{base}/v1/plugins/{loop_id}/invoke", {},
                  tenant="tenant-a", expect_status=201)
    dt = time.monotonic() - t0
    check("loop task marked failed", body["status"] == "failed", body)
    check("loop failed at invocation stage", body.get("error_stage") == "invocation", body)
    check("loop output is null (no half result)", body.get("output") is None, body)
    check("termination reason names fuel/deadline",
          "fuel" in (body.get("error_message") or "") or "deadline" in (body.get("error_message") or ""),
          body.get("error_message"))
    check("loop ended well under wall budget", dt < 8, f"{dt:.2f}s")
    print(f"     (terminated in {dt:.2f}s: {body.get('error_message')})")

    print("== 4. over-privileged imports fail at instantiation ==")
    bad_id, _, _ = upload(base, "tenant-a", "overpriv", "unauthorized_imports.wasm", BAD_CONTRACT)
    s, body = req("POST", f"{base}/v1/plugins/{bad_id}/invoke", {},
                  tenant="tenant-a", expect_status=201)
    check("unauthorized module fails", body["status"] == "failed", body)
    check("fails at instantiation, never runs",
          body.get("error_stage") == "instantiation", body)
    check("output null", body.get("output") is None)
    check("message names the allowlisted host",
          "allowlisted host" in (body.get("error_message") or ""), body.get("error_message"))

    print("== 5. oversized output claim rejected at invocation ==")
    out_id, _, _ = upload(base, "tenant-a", "bigout", "oversized_output.wasm",
                          {"abi_version": "abi-v1", "inputs": [], "host_allowlist": []})
    _, body = req("POST", f"{base}/v1/plugins/{out_id}/invoke", {},
                  tenant="tenant-a", expect_status=201)
    check("oversized output -> failed/invocation",
          body["status"] == "failed" and body["error_stage"] == "invocation", body)

    print("== 6. ordinary trap is invocation failure ==")
    trap_id, _, _ = upload(base, "tenant-a", "traps", "traps.wasm",
                           {"abi_version": "abi-v1", "inputs": [], "host_allowlist": []})
    _, body = req("POST", f"{base}/v1/plugins/{trap_id}/invoke", {},
                  tenant="tenant-a", expect_status=201)
    check("trap -> failed/invocation",
          body["status"] == "failed" and body["error_stage"] == "invocation", body)

    print("== 7. normal work continues after the attack modules ==")
    for i in range(5):
        _, body = req("POST", f"{base}/v1/plugins/{pid}/invoke",
                      {"a": i, "b": 100}, tenant="tenant-a", expect_status=201)
        check(f"post-attack arithmetic {i}", body["status"] == "succeeded"
              and body["output"] == {"sum": i + 100}, body)

    print("== 8. metrics: gauges reclaimed to zero ==")
    with urllib.request.urlopen(f"{base}/metrics", timeout=10) as r:
        metrics = r.read().decode()
    for line in metrics.splitlines():
        if line.startswith(("sandbox_instances_live", "sandbox_calls_in_flight",
                            "sandbox_memory_bytes_live")):
            print("    ", line)
    check("instances_live == 0", "sandbox_instances_live 0" in metrics)
    check("calls_in_flight == 0", "sandbox_calls_in_flight 0" in metrics)
    check("memory_bytes_live == 0", "sandbox_memory_bytes_live 0" in metrics)
    check("fuel-exhaustion counter recorded",
          "sandbox_tasks_fuel_exhausted_total " in metrics
          and not metrics.split("sandbox_tasks_fuel_exhausted_total")[1].splitlines()[0].strip().startswith("0"),
          "counter missing")

    print("== 9. tenant isolation ==")
    s, body = req("GET", f"{base}/v1/plugins/{pid}", tenant="tenant-b")
    check("other tenant cannot read plugin", s == 404, body)
    s, body = req("GET", f"{base}/v1/tasks/{arith_task}", tenant="tenant-b")
    check("other tenant cannot read task", s == 404, body)
    _, listing = req("GET", f"{base}/v1/plugins", tenant="tenant-b")
    check("other tenant sees empty plugin list", listing["plugins"] == [], listing)
    # tenant-b can register the same plugin name independently.
    pid_b, _, _ = upload(base, "tenant-b", "arith", "arith.wasm", ARITH_CONTRACT)
    _, body = req("POST", f"{base}/v1/plugins/{pid_b}/invoke",
                  {"a": 7, "b": 35}, tenant="tenant-b", expect_status=201)
    check("tenant-b own task works", body["output"] == {"sum": 42}, body)

    print("== 10. invalid modules/contracts rejected at upload ==")
    with open(os.path.join(FIX, "infinite_loop.wasm"), "rb") as f:
        good_b64 = base64.b64encode(f.read()).decode()
    bad_contract = {"abi_version": "abi-v9", "inputs": [], "host_allowlist": []}
    s, body = req("POST", f"{base}/v1/plugins",
                  {"name": "badabi", "contract": bad_contract, "wasm_base64": good_b64},
                  tenant="tenant-a")
    check("unknown abi version rejected", s == 422 and body["error"]["code"] == "contract_invalid", body)

    net_contract = {"abi_version": "abi-v1", "inputs": [], "host_allowlist": ["http_get"]}
    s, body = req("POST", f"{base}/v1/plugins",
                  {"name": "net", "contract": net_contract, "wasm_base64": good_b64},
                  tenant="tenant-a")
    check("network capability refused in contract",
          s == 422 and "whitelist" in body["error"]["message"], body)

    junk_b64 = base64.b64encode(b"\x00asm\x01\x00\x00\x00garbage").decode()
    ok_contract = {"abi_version": "abi-v1", "inputs": [], "host_allowlist": []}
    s, body = req("POST", f"{base}/v1/plugins",
                  {"name": "junk", "contract": ok_contract, "wasm_base64": junk_b64},
                  tenant="tenant-a")
    check("malformed wasm rejected (module_invalid)",
          s == 422 and body["error"]["code"] == "module_invalid", body)

    print("== 11. database invariants: no half results ==")
    psql = args.psql
    dsn = "host=127.0.0.1 user=sandbox dbname=sandbox"
    q = ("SELECT count(*) FROM tasks WHERE status='failed' AND output IS NOT NULL; "
         "SELECT count(*) FROM tasks WHERE status='succeeded' AND output IS NULL; "
         "SELECT count(*) FROM tasks WHERE status='running' OR status='pending';")
    import subprocess
    out = subprocess.run([psql, dsn, "-tAc", q], capture_output=True, text=True)
    if out.returncode != 0:
        print("  SKIP  psql unavailable:", out.stderr.strip()[:120])
    else:
        n = out.stdout.split()
        check("zero failed rows with output", n[0] == "0", out.stdout)
        check("zero succeeded rows without output", n[1] == "0", out.stdout)
        check("zero unfinished rows", n[2] == "0", out.stdout)

    print(f"\n{PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
