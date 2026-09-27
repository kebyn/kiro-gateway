# Kiro Gateway v2 质量审查与修复记录

审查范围是单租户网关的 Rust 服务、Admin 前端构建、三套协议适配层、Kiro 上游传输和
Responses 本地存储。本文不包含凭据、请求正文、真实数据库内容或任何密钥。当前修复批次
在 2026-09-27 完成，基线之后的相关提交为：

- `9a34bf9`：保留模型目录、凭据、上游和存储错误语义，Responses SSE 持久化失败停止继续发送。
- `3d8e4a4`：拒绝损坏续传历史，严格校验 v2 SQLite schema 和 WAL/SHM 权限。
- `fc33613`：所有成功 JSON/模型目录响应都使用有界逐 chunk 读取。
- `eac31df`：补充存储不可用、协议错误 envelope、损坏历史和传输边界测试。

## 工具链与基线

- Rust `1.85.1`（`rustfmt`、Clippy），pnpm `9.15.4`（Corepack 固定版本）。
- `cargo-audit 0.22.0`、`cargo-deny 0.20.2`。
- `cargo fmt --check`、严格 Clippy 和 Rust 测试通过；当前 Rust 测试为 `179 passed; 2 ignored`，
  CLI 配置集成为 `9 passed`。
- 忽略的测试只访问调用者显式指定的真实 Kiro CLI SQLite 或实时上游；普通 fixture 不含真实令牌。
- Corepack/pnpm 仍可能输出 Node `url.parse()` deprecation warning，来源是工具自身，不是项目代码。

真实 SQLite 文件只做权限/元数据检查，不在仓库内读取、复制或修改。若文件权限不是服务用户独占，
应由凭据所有者改为 `0600`；本项目不会替用户修复仓库外文件。

## 审查结论

### Critical

未发现可直接远程利用、无条件覆盖旧数据库或把访问令牌写入日志的 Critical 问题。

### High（已修复或降级）

1. **Responses 流事件持久化错误被忽略（已修复）**：`src/http/responses/live.rs` 原先大量
   `if let Ok(...)` 会在 append/transition 失败后继续发送事件。现在所有事件先经统一宏持久化；失败时
   发送 `response.incomplete`，错误码为 `storage_error`，记录一次 best-effort 不完整转换并立即停止。
   断连仍由析构 guard 排队转换，actor 不可用时会记录结构化错误。
2. **续传历史损坏静默变空（已修复）**：`ResponseStore::extract_messages/tools/opaque_history` 现在
   对缺失字段和反序列化错误返回 `AppError::Storage`。`previous_response_id` 不会再把损坏上下文当作空历史。
3. **v2 数据库校验过浅（已修复）**：启动时要求唯一的五张 v2 表、完整列/类型/非空/主键约束、
   `schema_meta` 单行 `CHECK(version=2)`、三条级联外键和事件唯一约束。旧库、额外表、缺列或错误约束
   均拒绝，且不会迁移或覆盖文件。
4. **上游体积限制绕过（已修复）**：模型目录和成功 JSON 响应不再调用无界 `bytes()`；无
   `Content-Length`、chunked 和 HTTP/2 数据均逐 chunk 受 `max_upstream_body_bytes` 限制。EventStream
   和非 2xx 预览保持原有上限。
5. **模型目录/凭据错误被误报为 400（已修复）**：模型不存在仍为协议级 `400`；凭据、网络、上游和
   完整性错误保留 `502`（凭据/上游 envelope），不会伪装成“模型不可用”。`GET /v1/models` 的兼容约定
   仍是在发现失败时返回 `200` 且 `data=[]`。

### Medium（仍需后续架构工作）

1. `src/http/responses/live.rs`、`src/upstream/request.rs` 仍同时包含编排、编码、累积和测试；当前
   状态机已有边界测试，但后续可按生命周期/编码/transport 再拆分。
2. Anthropic、Chat Completions、Responses handler 仍各自映射 SSE 终止事件；应继续抽出统一 generation
   service，避免三处策略漂移。
3. Responses actor 已将 SQLite I/O 移出 Tokio worker，transition 在单事务中写快照和事件；断连时的
   最后一次排队写入仍是 best-effort，进程在 actor 执行前退出时可能只留下 `in_progress` 记录。
4. opaque Codex 历史仍以受 allowlist 约束的 JSON 保存；未知 item type 会拒绝，但领域类型还可以进一步
   收紧。

### Low

1. 生产模块仍包含较多内嵌测试，后续可迁移到独立测试模块。
2. 固定 pnpm 的 Node deprecation warning 需要等待工具链升级窗口。

## 受控兼容性变化

- 发送生成请求时，模型目录请求失败或凭据不可用现在返回 `502` 协议错误 envelope；只有明确不存在的
  模型仍返回 `400 invalid_request`。
- 使用 `previous_response_id` 读取到缺失/损坏的 `messages`、`tools` 或 `opaque_history` 时返回
  `500`（内部存储错误），不再静默继续。
- Responses SSE 的本地事件追加失败会发送 `response.incomplete`，其 `error.code` 为
  `storage_error`，然后关闭流；后续事件不会声称已可靠存储。
- v2 存储只接受严格 schema。旧 schema、缺列、额外表、错误外键/唯一约束都必须迁移到新路径，网关
  不会自动迁移、删除或覆盖旧文件。

## 测试与证据矩阵

已执行并通过：

- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `cargo test --locked --all-features`：179 passed、2 ignored；`tests/generate_config_cli.rs`：9 passed。
- Responses：普通/SSE、工具并行与续传、事件顺序、提前 EOF、不完整终止、断连持久化、actor 不可用和
  损坏历史。
- 上游：EventStream CRC/截断/重试、成功 JSON 空响应、chunked 超限、声明长度截断、连接中断重试和
  非 2xx 预览上限。
- SQLite：旧库字节不变拒绝、缺列 schema 拒绝、外键/唯一约束、事务事件顺序、主库及 WAL/SHM 权限。
- 三套协议错误 envelope 均验证了 `400` 请求错误与 `502` 凭据/上游错误的区分。

最终验收已在当前工作树执行并记录：

```text
make check
make test
make package
make audit
make deny
make reproducible
make docker
docker compose config   # 使用临时脱敏 .env
make secrets
git diff --check
```

结果：上述命令全部通过；`make test` 为 Rust `179 passed; 2 ignored` 加 CLI `9 passed`。
`make reproducible` 的两次隔离 release 二进制 SHA-256 均为
`f79ef23038ae2c01087a630a282190c08b1ad43e44deb5e15d02ae4dcf998b6e`。
`make docker` 成功，镜像 manifest digest 为
`sha256:42b0f99d16f0a18048b229fedb39146c8e2881ec73766586fa4926884088c750`，容器以 UID `10001`
运行。使用临时脱敏 `.env` 的 `docker compose config`、配置生成/`--check-config`、旧配置拒绝、
`make secrets` 和 `git diff --check` 均通过；临时文件已删除，工作树干净。

`KIRO_ALLOW_LIVE_TESTS=1` 加只读 `/root/.local/share/kiro-cli/data.sqlite3`（未配置 JSON 回写）运行
`scripts/test-local-clients.sh`，动态模型 `gpt-5.6-sol`，健康/鉴权/模型发现、三套协议普通响应与
SSE、Codex、Claude Code、Grok Build 共 `12 passed; 0 failed`。客户端版本为 Codex `0.157.1`、
Claude Code `2.1.274`、Grok `1.0.41`。

外部/工具 warning：Corepack pnpm 输出 Node `url.parse()` deprecation；Docker builder 在镜像内没有
Node 时使用仓库已有 `admin-ui/dist`（不影响构建结果）。真实 SQLite 当前 `0644 root:root`，仅做
`stat` 检查且未修改；建议凭据所有者改为 `0600`。最终提交为 `ac89d67`。
