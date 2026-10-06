//! End-to-end acceptance test. Boots the real server against PostgreSQL and
//! runs the acceptance scenario:
//!
//!   * pure arithmetic plugin succeeds (concurrently with the others),
//!   * an intentional infinite loop is terminated and reported as
//!     `resource_exhausted` while normal tasks keep flowing,
//!   * a module importing non-whitelisted host functions is rejected at
//!     upload (`module_validation_failed`),
//!   * instantiation and invocation failures are reported distinctly,
//!   * failed tasks leave no partial success behind,
//!   * after everything ends, execution resources are reclaimed
//!     (`active_executions == 0`),
//!   * tenants cannot see each other's tasks.
//!
//! Requires DATABASE_URL (default: postgres://postgres@127.0.0.1:54329/pluginhost_test).

use plugin_host::{build_state, config::AppConfig, routes};
use serde_json::{json, Value};

const TENANT_A: &str = "acme";
const TENANT_B: &str = "globex";

struct Server {
    base: String,
    client: reqwest::Client,
}

async fn start() -> Server {
    let mut config = AppConfig::from_env();
    config.database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres@127.0.0.1:54329/pluginhost_test".to_string());
    let state = build_state(config).await.expect("build app state");
    sqlx::query("TRUNCATE tasks, plugins")
        .execute(&state.db)
        .await
        .expect("clean slate");
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
    }
}

fn contract() -> Value {
    json!({
        "type": "object",
        "properties": {
            "a": { "type": "integer" },
            "b": { "type": "integer" }
        },
        "required": ["a", "b"],
        "additionalProperties": false
    })
}

fn wasm(wat_source: &str) -> String {
    let bytes = wat::parse_str(wat_source).expect("valid WAT");
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
}

async fn upload(s: &Server, tenant: &str, name: &str, wasm_b64: String) -> (u16, Value) {
    let res = s
        .client
        .post(format!("{}/v1/plugins", s.base))
        .header("x-tenant-id", tenant)
        .json(&json!({
            "name": name,
            "contract": contract(),
            "wasm_base64": wasm_b64,
        }))
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = res.json::<Value>().await.unwrap();
    (status, body)
}

async fn create_task(s: &Server, tenant: &str, plugin_id: &str, input: Value) -> (u16, Value) {
    let res = s
        .client
        .post(format!("{}/v1/plugins/{plugin_id}/tasks", s.base))
        .header("x-tenant-id", tenant)
        .json(&json!({ "input": input }))
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = res.json::<Value>().await.unwrap();
    (status, body)
}

async fn get_task(s: &Server, tenant: &str, task_id: &str) -> (u16, Value) {
    let res = s
        .client
        .get(format!("{}/v1/tasks/{task_id}", s.base))
        .header("x-tenant-id", tenant)
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = res.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn wait_task(s: &Server, tenant: &str, task_id: &str) -> Value {
    for _ in 0..200 {
        let (_, body) = get_task(s, tenant, task_id).await;
        match body["status"].as_str() {
            Some("succeeded") | Some("failed") => return body,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    panic!("task {task_id} did not reach a terminal state in time");
}

#[tokio::test]
async fn acceptance() {
    let s = start().await;

    // --- health ---
    let res = s
        .client
        .get(format!("{}/healthz", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // --- 1. pure arithmetic plugin uploads and validates ---
    let (st, body) = upload(
        &s,
        TENANT_A,
        "arith",
        wasm(include_str!("../scripts/wat/arith.wat")),
    )
    .await;
    assert_eq!(st, 201, "arith upload: {body}");
    let arith_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(body["imports"], json!(["env.host_log"]));

    // --- 2. unauthorized imports are rejected at upload ---
    let (st, body) = upload(
        &s,
        TENANT_A,
        "rogue",
        wasm(include_str!("../scripts/wat/unauthorized.wat")),
    )
    .await;
    assert_eq!(st, 422, "rogue upload must be rejected: {body}");
    assert_eq!(body["error"]["kind"], "module_validation_failed");
    let disallowed = body["error"]["details"]["disallowed_imports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(disallowed.contains(&"wasi_snapshot_preview1.fd_write".to_string()));
    assert!(disallowed.contains(&"env.http_get".to_string()));

    // --- 3. contract violations are rejected before a task exists ---
    let (st, body) = create_task(&s, TENANT_A, &arith_id, json!({"a": 1})).await;
    assert_eq!(st, 422, "contract violation: {body}");
    assert_eq!(body["error"]["kind"], "contract_violation");

    // --- 4. infinite loop + arithmetic + memory hog run concurrently ---
    let (st, body) = upload(
        &s,
        TENANT_A,
        "loop",
        wasm(include_str!("../scripts/wat/infinite_loop.wat")),
    )
    .await;
    assert_eq!(st, 201, "loop upload: {body}");
    let loop_id = body["id"].as_str().unwrap().to_string();

    let (st, body) = upload(
        &s,
        TENANT_A,
        "memhog",
        wasm(include_str!("../scripts/wat/memory_hog.wat")),
    )
    .await;
    assert_eq!(st, 201, "memhog upload: {body}");
    let hog_id = body["id"].as_str().unwrap().to_string();

    let input = json!({"a": 20, "b": 22});
    let (st, body) = create_task(&s, TENANT_A, &loop_id, input.clone()).await;
    assert_eq!(st, 202, "{body}");
    let loop_task = body["id"].as_str().unwrap().to_string();

    let (st, body) = create_task(&s, TENANT_A, &arith_id, input.clone()).await;
    assert_eq!(st, 202, "{body}");
    let arith_task = body["id"].as_str().unwrap().to_string();

    let (st, body) = create_task(&s, TENANT_A, &hog_id, input.clone()).await;
    assert_eq!(st, 202, "{body}");
    let hog_task = body["id"].as_str().unwrap().to_string();

    let (r_loop, r_arith, r_hog) = tokio::join!(
        wait_task(&s, TENANT_A, &loop_task),
        wait_task(&s, TENANT_A, &arith_task),
        wait_task(&s, TENANT_A, &hog_task),
    );

    // The infinite loop is terminated, reported as resource exhaustion, and
    // leaves no partial result behind.
    assert_eq!(r_loop["status"], "failed", "{r_loop}");
    assert_eq!(r_loop["error"]["kind"], "resource_exhausted", "{r_loop}");
    assert!(r_loop["output"].is_null(), "{r_loop}");
    assert!(r_loop["finished_at"].is_string(), "{r_loop}");

    // Normal tasks kept running while the loop burned.
    assert_eq!(r_arith["status"], "succeeded", "{r_arith}");
    let expected_sum: u64 = serde_json::to_vec(&input)
        .unwrap()
        .iter()
        .map(|b| *b as u64)
        .sum();
    assert_eq!(
        r_arith["output"],
        json!({ "sum": expected_sum }),
        "{r_arith}"
    );
    assert!(r_arith["fuel_consumed"].as_i64().unwrap() > 0);
    assert!(r_arith["error"].is_null());

    // The memory limiter refused growth without trapping the task.
    assert_eq!(r_hog["status"], "succeeded", "{r_hog}");
    assert_eq!(r_hog["output"], json!({ "ok": true }), "{r_hog}");

    // --- 5. the host is still healthy after killing the loop ---
    let (st, body) = create_task(&s, TENANT_A, &arith_id, json!({"a": 1, "b": 2})).await;
    assert_eq!(st, 202);
    let again = wait_task(&s, TENANT_A, body["id"].as_str().unwrap()).await;
    assert_eq!(again["status"], "succeeded", "{again}");

    // --- 6. all execution resources were reclaimed ---
    let metrics: Value = s
        .client
        .get(format!("{}/v1/metrics", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metrics["active_executions"], 0, "{metrics}");

    // --- 7. instantiation failure is reported distinctly ---
    let (st, body) = upload(
        &s,
        TENANT_A,
        "bigmem",
        wasm(include_str!("../scripts/wat/bigmem.wat")),
    )
    .await;
    assert_eq!(st, 201, "bigmem is valid wasm, upload must pass: {body}");
    let (st, body) = create_task(&s, TENANT_A, body["id"].as_str().unwrap(), input.clone()).await;
    assert_eq!(st, 202, "{body}");
    let r = wait_task(&s, TENANT_A, body["id"].as_str().unwrap()).await;
    assert_eq!(r["status"], "failed", "{r}");
    assert_eq!(r["error"]["kind"], "instantiation_failed", "{r}");
    assert!(r["output"].is_null(), "{r}");

    // --- 8. invocation failure is reported distinctly ---
    let (st, body) = upload(
        &s,
        TENANT_A,
        "trap",
        wasm(include_str!("../scripts/wat/trap.wat")),
    )
    .await;
    assert_eq!(st, 201, "{body}");
    let (st, body) = create_task(&s, TENANT_A, body["id"].as_str().unwrap(), input.clone()).await;
    assert_eq!(st, 202, "{body}");
    let r = wait_task(&s, TENANT_A, body["id"].as_str().unwrap()).await;
    assert_eq!(r["status"], "failed", "{r}");
    assert_eq!(r["error"]["kind"], "invocation_failed", "{r}");
    assert!(r["output"].is_null(), "{r}");

    // --- 9. tenant isolation ---
    let (st, _) = get_task(&s, TENANT_B, &arith_task).await;
    assert_eq!(st, 404, "tenant B must not see tenant A's task");
    let res = s
        .client
        .get(format!("{}/v1/tasks/{arith_task}", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400, "missing tenant header must be rejected");
    let res = s
        .client
        .get(format!("{}/v1/plugins/{arith_id}", s.base))
        .header("x-tenant-id", TENANT_B)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404, "tenant B must not see tenant A's plugin");
}
