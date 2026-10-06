#!/usr/bin/env bash
#
# Acceptance run against a live server (default http://127.0.0.1:8080).
#
# Scenario:
#   1. pure arithmetic plugin runs fine
#   2. an intentional infinite loop is terminated (resource_exhausted) while
#      normal tasks keep flowing
#   3. a module importing non-whitelisted host functions is rejected at upload
#   4. instantiation / invocation failures are reported as distinct kinds
#   5. failed tasks leave no partial success behind
#   6. after everything ends, active_executions returns to 0
#   7. tenants cannot see each other's tasks
#
# Usage: BASE=http://127.0.0.1:8080 scripts/acceptance.sh
set -uo pipefail

BASE="${BASE:-http://127.0.0.1:8080}"
TA="acme"
TB="globex"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAT_DIR="$HERE/wat"
CONTRACT='{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false}'
INPUT='{"a":20,"b":22}'

PASS=0
FAIL=0
ok()  { PASS=$((PASS+1)); echo "  PASS  $*"; }
no()  { FAIL=$((FAIL+1)); echo "  FAIL  $*"; }
chk() { # chk <description> <actual> <expected>
  if [[ "$2" == "$3" ]]; then ok "$1"; else no "$1 (expected=$3 actual=$2)"; fi
}

TMP_BODY="$(mktemp)"
trap 'rm -f "$TMP_BODY"' EXIT

req() { # req <METHOD> <PATH> <TENANT|-> [JSON]
  local method="$1" path="$2" tenant="$3" data="${4:-}"
  local args=(-sS -X "$method" "$BASE$path" -H 'content-type: application/json')
  [[ "$tenant" != "-" ]] && args+=(-H "x-tenant-id: $tenant")
  [[ -n "$data" ]] && args+=(-d "$data")
  HTTP_CODE=$(curl "${args[@]}" -o "$TMP_BODY" -w '%{http_code}')
  HTTP_BODY=$(cat "$TMP_BODY")
}

jqget() { jq -r "$1" <<<"$HTTP_BODY" 2>/dev/null; }

upload() { # upload <name> <wat-file> -> HTTP_*
  local payload
  payload=$(jq -n --arg name "$1" --arg wasm "$(base64 -w0 "$WAT_DIR/$2")" \
              --argjson contract "$CONTRACT" \
              '{name:$name, contract:$contract, wasm_base64:$wasm}')
  req POST /v1/plugins "$TA" "$payload"
}

create_task() { # create_task <plugin_id> -> HTTP_*
  req POST "/v1/plugins/$1/tasks" "$TA" "{\"input\": $INPUT}"
}

wait_task() { # wait_task <task_id> <tenant> -> TASK_BODY
  local id="$1" tenant="$2" status=""
  for _ in $(seq 1 300); do
    req GET "/v1/tasks/$id" "$tenant"
    status=$(jqget .status)
    if [[ "$status" == "succeeded" || "$status" == "failed" ]]; then
      TASK_BODY="$HTTP_BODY"
      return 0
    fi
    sleep 0.1
  done
  TASK_BODY='{"status":"timeout"}'
  return 1
}

echo "== 0. health =="
req GET /healthz -
chk "GET /healthz -> 200" "$HTTP_CODE" "200"

echo "== 1. pure arithmetic plugin =="
upload arith arith.wat
chk "upload arith -> 201" "$HTTP_CODE" "201"
ARITH_ID=$(jqget .id)
chk "digest recorded (64 hex chars)" "$(jqget .sha256 | wc -c | tr -d ' ')" "65"
chk "imports list shows only whitelisted host_log" "$(jqget '.imports | join(",")')" "env.host_log"

echo "== 2. unauthorized module rejected at upload =="
upload rogue unauthorized.wat
chk "upload rogue -> 422" "$HTTP_CODE" "422"
chk "error kind is module_validation_failed" "$(jqget .error.kind)" "module_validation_failed"
DISALLOWED=$(jqget '.error.details.disallowed_imports | join(",")')
case "$DISALLOWED" in
  *wasi_snapshot_preview1.fd_write*env.http_get*|*env.http_get*wasi_snapshot_preview1.fd_write*)
    ok "disallowed imports reported: $DISALLOWED" ;;
  *) no "disallowed imports reported (got: $DISALLOWED)" ;;
esac

echo "== 3. contract violation rejected before a task exists =="
req POST "/v1/plugins/$ARITH_ID/tasks" "$TA" '{"input": {"a": 1}}'
chk "missing required field -> 422" "$HTTP_CODE" "422"
chk "error kind is contract_violation" "$(jqget .error.kind)" "contract_violation"

echo "== 4. concurrent: infinite loop + arithmetic + memory hog =="
upload looper infinite_loop.wat;  LOOP_ID=$(jqget .id)
chk "upload looper -> 201" "$HTTP_CODE" "201"
upload memhog memory_hog.wat;    HOG_ID=$(jqget .id)
chk "upload memhog -> 201" "$HTTP_CODE" "201"

create_task "$LOOP_ID";  LOOP_TASK=$(jqget .id);  chk "loop task accepted" "$HTTP_CODE" "202"
create_task "$ARITH_ID"; ARITH_TASK=$(jqget .id); chk "arith task accepted" "$HTTP_CODE" "202"
create_task "$HOG_ID";   HOG_TASK=$(jqget .id);   chk "memhog task accepted" "$HTTP_CODE" "202"

wait_task "$LOOP_TASK" "$TA";  R_LOOP="$TASK_BODY"
wait_task "$ARITH_TASK" "$TA"; R_ARITH="$TASK_BODY"
wait_task "$HOG_TASK" "$TA";   R_HOG="$TASK_BODY"

chk "infinite loop terminated as failed" "$(jq -r .status <<<"$R_LOOP")" "failed"
chk "loop error kind is resource_exhausted" "$(jq -r .error.kind <<<"$R_LOOP")" "resource_exhausted"
chk "loop left no output behind" "$(jq -r .output <<<"$R_LOOP")" "null"
chk "loop has finished_at" "$(jq -r '.finished_at != null' <<<"$R_LOOP")" "true"

EXPECTED_SUM=$(printf '%s' "$INPUT" | od -An -tu1 | tr -s ' ' | awk '{for(i=1;i<=NF;i++)s+=$i}END{print s}')
chk "arithmetic task succeeded" "$(jq -r .status <<<"$R_ARITH")" "succeeded"
chk "arithmetic output correct" "$(jq -r .output.sum <<<"$R_ARITH")" "$EXPECTED_SUM"
chk "arithmetic fuel accounted" "$(jq -r '.fuel_consumed > 0' <<<"$R_ARITH")" "true"

chk "memory hog task succeeded (growth refused, no trap)" "$(jq -r .status <<<"$R_HOG")" "succeeded"
chk "memory hog output ok" "$(jq -r .output.ok <<<"$R_HOG")" "true"

echo "== 5. host still healthy after killing the loop =="
create_task "$ARITH_ID"; AGAIN=$(jqget .id)
wait_task "$AGAIN" "$TA"
chk "arithmetic still succeeds" "$(jq -r .status <<<"$TASK_BODY")" "succeeded"

echo "== 6. resources reclaimed =="
req GET /v1/metrics -
chk "active_executions back to 0" "$(jqget .active_executions)" "0"

echo "== 7. distinct failure kinds =="
upload bigmem bigmem.wat; BIG_ID=$(jqget .id)
chk "bigmem uploads (valid wasm)" "$HTTP_CODE" "201"
create_task "$BIG_ID"; wait_task "$(jqget .id)" "$TA"
chk "oversized initial memory -> instantiation_failed" "$(jq -r .error.kind <<<"$TASK_BODY")" "instantiation_failed"
chk "instantiation failure left no output" "$(jq -r .output <<<"$TASK_BODY")" "null"

upload trapper trap.wat; TRAP_ID=$(jqget .id)
chk "trapper uploads" "$HTTP_CODE" "201"
create_task "$TRAP_ID"; wait_task "$(jqget .id)" "$TA"
chk "unreachable trap -> invocation_failed" "$(jq -r .error.kind <<<"$TASK_BODY")" "invocation_failed"
chk "invocation failure left no output" "$(jq -r .output <<<"$TASK_BODY")" "null"

echo "== 8. tenant isolation =="
req GET "/v1/tasks/$ARITH_TASK" "$TB"
chk "tenant B cannot read tenant A's task" "$HTTP_CODE" "404"
req GET "/v1/plugins/$ARITH_ID" "$TB"
chk "tenant B cannot read tenant A's plugin" "$HTTP_CODE" "404"
req GET "/v1/tasks/$ARITH_TASK" "-"
chk "missing tenant header -> 400" "$HTTP_CODE" "400"

echo
echo "==================================="
echo "acceptance: $PASS passed, $FAIL failed"
echo "==================================="
[[ "$FAIL" -eq 0 ]]
