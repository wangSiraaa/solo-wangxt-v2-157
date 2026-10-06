# plugin-host

第三方计算插件的**可执行、可终止**边界：Rust + Axum + Wasmtime 服务端 API（无前端），PostgreSQL 持久化插件摘要、接口契约与任务状态。

## 安全模型

| 维度 | 机制 |
| --- | --- |
| 宿主函数 | **显式白名单**：链接器只定义 `env.host_log`；不链接 WASI，默认禁止网络与任意文件访问。上传时校验 import 列表，越权模块直接拒绝 |
| 死循环 | **燃料（fuel）**：每条指令计费，耗尽即 trap；**纪元（epoch）墙钟**兜底；外层 tokio timeout 最后防线。死循环无法拖死宿主 |
| 内存 | `StoreLimits` 限制线性内存 / 表 / 实例数；初始内存超限 → 实例化失败；`memory.grow` 超限 → 返回 -1 |
| 输出 | 输出字节数上限，超限不读取直接判失败 |
| 并发 | 信号量限制并发执行数；执行在 `spawn_blocking` 隔离线程池 |
| 资源回收 | 每次执行独立 `Store`，结束（含 trap）即 drop，线性内存归还 OS；`/v1/metrics` 的 `active_executions` 归零可验证 |
| 数据完整性 | 任务终态**单条原子 UPDATE** + `tasks_outcome_integrity` CHECK 约束：失败任务不可能留下半份成功结果；崩溃遗留任务启动时 reconciler 标记 `interrupted` |
| 多租户 | 所有查询按 `tenant_id` 隔离；日志为 JSON 结构化，每条记录（含 guest 参数）只携带所属租户的 `tenant_id` |

## 失败分类（分别返回）

| 阶段 | `error.kind` | 触发 |
| --- | --- | --- |
| 模块校验 | `module_validation_failed` | 上传时：wasm 非法 / 越权 import / 缺少 ABI 导出 / 超限（HTTP 422） |
| 契约校验 | `contract_violation` | 输入不满足插件 JSON Schema（HTTP 422，不建任务） |
| 实例初始化 | `instantiation_failed` | 初始内存超限、start 函数 trap 等 |
| 调用 | `invocation_failed` | guest trap（如 `unreachable`）、输出非法 JSON 等 |
| 资源超限 | `resource_exhausted` | 燃料耗尽或墙钟超时（死循环） |
| 输出超限 | `output_limit_exceeded` | 输出超过限额 |
| 中断 | `interrupted` | 服务重启时仍在飞行的任务 |

## 插件 ABI

模块必须导出：

- `memory`：线性内存
- `alloc(len: i32) -> ptr: i32`：为输入分配缓冲区
- `run(ptr: i32, len: i32) -> i64`：执行；返回值打包 `(out_ptr << 32) | out_len`，输出必须是 UTF-8 JSON

可选 import（白名单全部内容）：`env.host_log(level: i32, ptr: i32, len: i32)`。

## API

所有业务接口需要请求头 `x-tenant-id`。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/healthz` | 存活探针 |
| GET | `/v1/metrics` | 执行器指标（含 `active_executions`） |
| POST | `/v1/plugins` | 上传插件 `{name, contract, wasm_base64}` → 201 + sha256 摘要 |
| GET | `/v1/plugins` | 列出本租户插件 |
| GET | `/v1/plugins/{id}` | 插件元数据 |
| POST | `/v1/plugins/{id}/tasks` | 创建任务 `{input}` → 202 + 任务 id |
| GET | `/v1/tasks/{id}` | 查询任务状态 / 输出 / 错误 |

## 运行

```bash
# 1. 启动 PostgreSQL（用户态二进制，无需 root；应用首连自动建库）
scripts/start-postgres.sh

# 2. 启动服务
DATABASE_URL=postgres://postgres@127.0.0.1:54329/pluginhost \
BIND_ADDR=127.0.0.1:8080 \
cargo run --bin plugin-host
```

限额均可通过环境变量覆盖（见 `src/config.rs`）：`LIMIT_FUEL`、`LIMIT_WALL_CLOCK_MS`、`LIMIT_MAX_MEMORY_BYTES`、`LIMIT_MAX_OUTPUT_BYTES`、`LIMIT_MAX_CONCURRENT_EXECUTIONS` 等。

## 验收

同时运行纯算术、故意死循环、越权访问三类模块，检查正常任务是否继续、超限任务能否终止、结束后资源是否真正回收：

```bash
# 方式一：shell 脚本（针对运行中的服务）
scripts/acceptance.sh

# 方式二：Rust 集成测试（自带服务实例）
DATABASE_URL=postgres://postgres@127.0.0.1:54329/pluginhost_test cargo test --test acceptance -- --nocapture
```

测试模块在 `scripts/wat/`：`arith.wat`（纯算术 + 白名单 host_log）、`infinite_loop.wat`（死循环）、`unauthorized.wat`（WASI/网络越权导入）、`bigmem.wat`（初始内存超限 → 实例化失败）、`trap.wat`（调用期 trap）、`memory_hog.wat`（内存增长至限额被拒）。

## 日志与租户隔离

JSON 结构化日志，每个请求一个 span（`tenant_id`、`request_id`）；任务执行在独立的 `task_execution` span 中。guest 通过 `host_log` 输出的每行都强制打上所属租户的 `tenant_id` 与 `task_id`；任务参数仅在 DEBUG 级、所属租户的上下文中出现，不会跨租户泄漏。guest 日志有行数与行长预算，防止日志洪泛。
