# Kiro Gateway v2 质量审查

本文记录 v2 重构开始前的基线、已确认风险和验收口径。审查日期为 2026-09-26，
基线提交为 `810b437`。记录不包含凭据、请求正文或数据库内容。

## 工具链与基线

- Rust `1.85.1`，minimal profile，包含 rustfmt 与 Clippy。
- pnpm `9.15.4`，通过 Corepack 和 `--frozen-lockfile` 安装。
- `cargo-audit 0.22.0` 与官方静态发布的 `cargo-deny 0.20.2`。二者均使用仓库固定的
  Cargo 1.85.1 检查本项目，并支持当前 advisory-db/CVSS 4.0 和 deny v2 配置。
- Admin 前端重新生成后无差异；`cargo fmt --check` 和严格 Clippy 通过。
- 单元测试 165 项，其中 163 项通过、2 项真实环境测试按设计忽略；CLI 集成测试
  7 项全部通过。
- `cargo package --locked --allow-dirty` 与 `cargo audit` 通过。
- 两次隔离 release 构建产物的 SHA-256 相同。旧检查把任意 `/tmp/` 字面量当成
  构建路径而误报，已改为只检查实际工作区和两个实际临时目录。
- 初次 Docker 构建在下载固定 builder 镜像层时收到 `tls: bad record MAC`，属于
  外部传输失败；代码构建尚未开始，后续验收必须重试并通过。
- 真实 Kiro CLI SQLite 仅以 `stat` 检查，当前为 `0644 root:root`。这是凭据泄露
  风险；本次工作不读取内容、不修改仓库外权限，建议所有者自行改为 `0600`。

Rust 1.85.1 能从源码编译的 `cargo-deny 0.18.3` 内置旧 RustSec 解析器，面对当前
CVSS 4.0 advisory 会记录解析错误却返回成功，不能提供可信结果。因此本机采用官方
SHA-256 校验过的 0.20.2 静态二进制；这不改变项目 Rust 工具链。未实际命中的
`LGPL-2.1-or-later` allow 项也已删除，依赖当前通过 MIT/Apache 分支满足政策。

## Critical

未发现可直接远程利用、会泄露凭据或会无条件破坏存储数据的 Critical 问题。

## High

1. Responses 存储在 Tokio 请求任务中通过 `std::sync::Mutex<rusqlite::Connection>`
   同步执行。锁等待、busy timeout 和磁盘 I/O 都可能阻塞运行时 worker，影响所有协议
   的流式响应和取消传播。
2. Response 快照更新与 lifecycle event 追加是两个独立写操作；进程退出或写失败可留下
   “最终快照/最终事件不一致”的记录。客户端断连依赖析构时另起异步任务，无法保证命令
   已被数据库持久化。
3. 旧库会被原地建表、删表及补列，没有 schema 世代标记。这与 v2 的破坏性存储边界
   冲突，也让误指向旧数据文件时发生不可逆修改。
4. 内部消息以字符串角色和任意 `serde_json::Value` 表达；协议验证、工具配对与上游
   payload 构造在不同模块重复解释 JSON，存在接受后再丢失语义或把未知内容误当文本的
   风险。

## Medium

1. `http/responses.rs` 超过 2,000 行，同时承担请求编排、实时状态、SSE 编码、payload
   生成、存储和大量测试；断连、上游 EOF、工具增量和最终状态之间的约束难以独立验证。
2. `upstream/request.rs` 超过 1,300 行，同时负责 HTTP、EventStream、JSON 探测、重试、
   完整性和结果累积；传输错误与协议不完整的边界容易耦合。
3. 三个 HTTP handler 各自驱动上游流并映射终止状态，重试、取消和完整性策略没有统一的
   应用服务入口。
4. 平铺配置和凭据环境变量分散在配置与 auth 模块，客户端网关密钥、上游 API Key 和
   OIDC 元数据缺少结构化命名空间，部署时容易混淆。
5. Responses opaque 历史与普通输入共用开放 JSON 形态；虽然当前有 allowlist，类型系统
   无法阻止它进入上游转换路径。

## Low

1. 生产模块中内嵌数千行测试，增加导航和所有权模糊度；应迁入对应子模块测试文件。
2. 旧可复现构建检查对通用 `/tmp/` 字面量误报，不能准确证明路径泄漏。
3. 缺少统一的仓库秘密检查入口。v2 基线新增对危险文件名、SQLite 文件头、PEM 私钥、
   常见访问令牌形态的保守检查及自测试；占位符不会被误判。
4. Corepack 运行 pnpm 时 Node 会打印 `url.parse()` deprecation warning，来自工具自身，
   不影响生成一致性，但应持续关注固定 pnpm 的升级窗口。

## 重构验收映射

- 类型化领域模型消除 High-4、Medium-5。
- 上游 transport/decoder/policy/accumulator 拆分消除 Medium-2，并由统一 generation
  service 消除 Medium-3。
- Responses 编排、状态机、编码、payload 与存储生命周期拆分消除 Medium-1。
- 单连接异步 SQLite actor、schema v2 和事务化 transition 消除 High-1 至 High-3。
- v2 分组配置和双下划线环境变量消除 Medium-4；旧 JSON、旧变量和旧数据库均明确拒绝。
- 最终验收必须覆盖普通响应、SSE、并行工具、续传、提前 EOF、断连、体积限制、依赖
  策略、可复现构建、Docker，以及在不输出真实凭据前提下的本地客户端冒烟。

## v2 最终验证记录

以下命令在代码最终提交（`ade24cf`；不含本节后续文档提交）上通过；验证过程未输出真实凭据或请求正文：

- `make check`：前端生成一致、秘密扫描、格式检查和严格 Clippy 通过。
- `make test`：Rust 单元/集成测试 165 项通过、2 项真实环境测试按设计忽略；配置 CLI
  测试 9 项通过。
- 上游非成功响应的错误正文按 `max_upstream_body_bytes` 及 4 KiB 预览上限流式读取；回归
  测试覆盖超限错误正文不会被无界缓冲。
- `make package`、`make audit`、`make deny`：打包、RustSec advisory、许可证/来源策略通过。
- `make reproducible`：两次隔离 release 构建 SHA-256 均为
  `056e6ccbc750350b822a07b6180c61f41b78791d5d81148c28699ec118241ef2`，并通过绝对路径检查。
  路径检查使用目录分隔符匹配，避免仓库根目录名恰好是 `/data` 时误匹配
  `~/.local/share/kiro-cli/data.sqlite3` 等正常字符串。
- `make docker`：固定 Rust/Debian digest 镜像构建成功，镜像以 UID `10001` 非 root 运行。
- `docker compose config`：使用脱敏占位 `.env` 临时夹具校验成功；夹具未保留在工作树。
- `scripts/test-local-clients.sh`：使用真实 SQLite 凭据只读启动临时网关，未配置 JSON 回写；
  鉴权、模型发现、三套协议的普通响应和 SSE，以及 Codex、Claude Code、Grok Build 客户端共
  12 项检查全部通过。临时响应数据库和诊断目录已删除。

仓库外的 `/root/.local/share/kiro-cli/data.sqlite3` 在真实冒烟期间由 SQLite 来源只读打开，未
写入或修改权限；当前权限为 `0644 root:root`，仍建议凭据所有者改为 `0600`。
