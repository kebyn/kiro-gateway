# kiro-gateway

单租户 Kiro API 网关，提供 Anthropic Messages、OpenAI Chat Completions 和 OpenAI Responses 兼容接口。服务只选择一个凭据，不包含账号池、轮询或故障切换逻辑。

## 功能

- Anthropic Messages：普通响应、SSE、Token 估算、Tool Call 与 Tool Result 续传。
- OpenAI Chat Completions：普通响应、SSE、并行 `tool_calls` 与 `tool` 消息续传。
- OpenAI Responses：普通响应、SSE、`function_call` / `function_call_output`、Codex
  `custom_tool_call` / `custom_tool_call_output`、本地响应存储和
  `previous_response_id`；兼容 Codex 重放的 opaque 历史项，但不会将其转发给上游模型。
- Kiro IDE 与 CLI 两种上游端点格式。
- 环境变量、JSON、只读 SQLite 和 API Key 凭据来源。
- 凭据自动刷新、Admin 会话与 CSRF 防护。
- 固定 Rust 工具链和可复现构建检查。

## 安装与启动

需要 Rust 1.85.1。仓库中的 `rust-toolchain.toml` 会让 rustup 自动选择该版本。

### 使用环境变量凭据

此示例使用运行时默认配置，Admin 默认关闭；只有将 `admin.enabled` 设为 `true` 时才需要 `KIRO_ADMIN_API_KEY`。

```sh
cp config.example.json config.json
chmod 600 config.json

export KIRO_CLIENT_API_KEY='client-key-change-me'
export KIRO_CREDENTIAL_SOURCE='env'
export KIRO_ACCESS_TOKEN='access-token-from-your-provider'

cargo run --locked -- --config config.json
```

服务默认监听 `127.0.0.1:8990`。确认服务状态：

```sh
curl http://127.0.0.1:8990/health
curl -H 'x-api-key: client-key-change-me' \
  http://127.0.0.1:8990/v1/models
```

构建发布二进制：

```sh
cargo build --release --locked
./target/release/kiro-gateway --config config.json
```

`--config` 也可通过 `KIRO_CONFIG` 指定。`--check-config` 会解析并打印最终配置后退出；敏感字段会显示为 `[REDACTED]`，但仍不要把输出写入共享日志。

### 生成网关配置

`--generate-config [PATH]` 会生成一份完整的 JSON 配置，其中包含两组由操作系统密码学随机源生成的、彼此独立的网关密钥，并默认启用 Admin：

```sh
cargo run --locked -- --generate-config config.json
export KIRO_ACCESS_TOKEN='access-token-from-your-provider'
cargo run --locked -- --config config.json
```

生成的文件只会填入网关的 `client_api_key` 和 `admin_api_key`，不会也无法替你生成上游 Kiro 凭据。启动前至少设置
`KIRO_ACCESS_TOKEN`、`KIRO_REFRESH_TOKEN` 或 `KIRO_API_KEY` 其中一种（也可以把配置改为 JSON/SQLite 凭据来源）。生成文件含有敏感密钥，程序会以 `0600` 权限独占创建；目标文件已存在时不会覆盖。省略路径时配置会完整写到 stdout，使用重定向保存时请由调用者自行保护文件权限：

```sh
cargo run --locked -- --generate-config > config.json
chmod 600 config.json
```

运行时的 `AppConfig` 默认值仍为 `admin.enabled=false`。因此不使用生成配置时，只设置客户端密钥和上游凭据即可启动；只有显式启用 Admin（包括使用生成配置）时才需要 `admin_api_key`。`--generate-config` 与 `--check-config` 不能同时使用。

### Docker

```sh
docker build -t kiro-gateway .
docker run --rm -p 8990:8990 \
  -e KIRO_CLIENT_API_KEY='client-key-change-me' \
  -e KIRO_ADMIN_API_KEY='admin-key-change-me' \
  -e KIRO_ADMIN_ENABLED='true' \
  -e KIRO_CREDENTIAL_SOURCE='env' \
  -e KIRO_ACCESS_TOKEN='access-token-from-your-provider' \
  kiro-gateway
```

镜像以 UID `10001` 的非 root 用户运行，并设置 `KIRO_HOST=0.0.0.0`。如使用 JSON/SQLite 凭据或需要保留 Responses 数据库，请挂载对应文件或目录，并确保该用户具有所需的读写权限。

CI 会将镜像发布到 GitHub Container Registry：

```sh
docker pull ghcr.io/kebyn/kiro-gateway:latest
docker run --rm -p 8990:8990 \
  -e KIRO_CLIENT_API_KEY='client-key-change-me' \
  -e KIRO_ADMIN_API_KEY='admin-key-change-me' \
  -e KIRO_ADMIN_ENABLED='true' \
  -e KIRO_CREDENTIAL_SOURCE='env' \
  -e KIRO_ACCESS_TOKEN='access-token-from-your-provider' \
  ghcr.io/kebyn/kiro-gateway:latest
```

推送到 `master` 会更新 `latest`；推送 Git tag 时会同时发布同名镜像 tag 和 `latest`，例如
`ghcr.io/kebyn/kiro-gateway:v1.0.0`。Pull Request 和其他分支只执行镜像构建校验，不会发布。

### Docker Compose（host 网络）

仓库中的 [`compose.yaml`](compose.yaml) 直接使用 GHCR 发布的镜像，并采用 Docker
`host` 网络模式。模板把网关固定绑定到宿主机的 `127.0.0.1:8990`，没有 `ports` 映射，
因此局域网和公网主机不能直接连接这个服务。

先准备配置、环境变量和 Responses 数据目录：

```sh
cp config.example.json config.json
cp .env.example .env
chmod 600 .env
mkdir -p data
```

编辑 `.env`，替换客户端密钥、Admin 密钥和上游凭据占位符。`.env.example` 默认设置
`KIRO_CREDENTIAL_SOURCE=env`；`KIRO_API_KEY` 不能与 `KIRO_ACCESS_TOKEN` 或
`KIRO_REFRESH_TOKEN` 同时填写，后两者可按上游要求一起配置。镜像以 UID `10001` 的
非 root 用户运行，启动前请确保该 UID 能读取 `config.json` 并写入 `data/`。

`config.example.json` 默认关闭 Admin；如需在 Compose 中启用 Admin，请在 `.env` 中同时设置
`KIRO_ADMIN_ENABLED=true` 和 `KIRO_ADMIN_API_KEY`，或直接把 `config.json` 的
`admin.enabled` 改为 `true`。

例如在 Linux 上可以使用：

```sh
sudo chown 10001:10001 config.json data
chmod 600 config.json
chmod 700 data
```

检查合并后的 Compose 配置并启动服务：

```sh
docker compose config
docker compose up -d
docker compose logs -f kiro-gateway
```

健康检查和停止命令：

```sh
curl --fail http://127.0.0.1:8990/health
docker compose down
```

由于服务只监听宿主机回环地址，需要对外提供访问时，应在同一宿主机上终止 TLS 的
反向代理，并将请求转发到 `http://127.0.0.1:8990`。不要在 Compose 中添加 `ports`
映射或把 `KIRO_HOST` 改为 `0.0.0.0`，除非确实要改变这个本机访问边界。

默认凭据来自 `.env`。如果使用 JSON 或 Kiro CLI SQLite 凭据，需要在 `compose.yaml`
的 `volumes` 中额外挂载文件，并把 `config.json` 中的来源和容器内路径改为对应值，
例如：

```yaml
volumes:
  - ./credentials/kiro.json:/etc/kiro-gateway/kiro.json:ro
  - ${HOME}/.local/share/kiro-cli/data.sqlite3:/var/lib/kiro-gateway/kiro-cli.sqlite3:ro
```

JSON 凭据使用 `credential_source: "json"` 和 `/etc/kiro-gateway/kiro.json`，SQLite
凭据使用 `credential_source: "sqlite"` 和 `/var/lib/kiro-gateway/kiro-cli.sqlite3`。
只读挂载适合读取现有凭据；如果需要把刷新后的 JSON 原子写回文件，应改用受控的可写
挂载，并确保 UID `10001` 具有文件权限。`data/` 中的 Responses SQLite 会保存请求、
输出和工具数据明文，请限制目录权限并按保留策略备份或删除。

## 接口与鉴权

`GET /health` 无需鉴权。所有 `/v1/*` 接口接受以下任一请求头，值必须与 `client_api_key` 一致：

```http
x-api-key: client-key-change-me
```

```http
Authorization: Bearer client-key-change-me
```

若同时提供，`x-api-key` 优先。密钥使用常量时间比较。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/health` | 服务健康状态和构建时间戳，无需鉴权 |
| `GET` | `/v1/models` | 返回兼容模型列表 |
| `POST` | `/v1/messages` | Anthropic Messages |
| `POST` | `/v1/messages/count_tokens` | 按字符数估算输入 Token，不是精确 tokenizer |
| `POST` | `/v1/chat/completions` | OpenAI Chat Completions |
| `POST` | `/v1/responses` | 创建 OpenAI Response |
| `GET` | `/v1/responses/{id}` | 读取本地存储的 Response |
| `DELETE` | `/v1/responses/{id}` | 删除本地存储的 Response，成功返回 `204` |

三套生成接口均支持 `stream: true`，响应类型为 SSE；流式响应会发送 keep-alive 注释，
并在完成时发送各自协议要求的终止事件（Anthropic `message_stop`、Chat Completions
`[DONE]`、Responses `response.completed` 或 `response.incomplete`）。

### 兼容性边界

- Anthropic `system`、OpenAI Chat 的 `system`/`developer` 消息以及 Responses
  `instructions`/对应消息会被合并到当前 Kiro 提示中，不会作为普通 user 消息发送。
- 输入消息目前只支持文本、thinking/reasoning 文本和工具调用/结果。图片、文档、
  `redacted_thinking` 及其他无法表达为文本的块会返回 `400`，不会静默丢失。
- Kiro 上游没有等价的强制工具选择语义；`tool_choice` 仅支持 `auto`，指定
  `required`、`any` 或具体工具会返回 `400`，而不是被忽略。
- Responses 的 custom grammar 工具会转换为 Kiro 可接受的字符串输入工具；自由格式
  `input` 会原样保留，并在响应中恢复为 `custom_tool_call`。命名空间工具会在发往
  Kiro 时将名称中的点号或命名空间分隔符编码为下划线，再在 Responses 输出中恢复
  原始名称和命名空间。
- Codex 的搜索、MCP、shell、图片生成、压缩和配置标记等没有 Kiro 等价执行语义的已知
  历史项会作为 opaque 历史接受并忽略；完全未知的 Responses item type 仍返回 `400`。
- `max_tokens`、`temperature` 等生成控制字段会被协议层接受，但当前 Kiro 请求格式没有
 可靠的一一映射；不要把它们视为精确生效的上游限制。

### Claude Code 流式调试

Claude Code 使用 Anthropic Messages 接口时，应将 `ANTHROPIC_BASE_URL` 指向网关地址，
并使用网关的客户端密钥作为 `ANTHROPIC_API_KEY`。如果客户端一直等待，可先用同一模型
执行最小流式请求，确认网关返回 `message_start`、内容块、`message_delta` 和
`message_stop`：

```sh
RUST_LOG=debug cargo run --locked -- --config config.json

curl -N -i \
  -H 'x-api-key: client-key-change-me' \
  -H 'content-type: application/json' \
  -H 'accept: text/event-stream' \
  http://127.0.0.1:8990/v1/messages \
  -d '{"model":"MODEL_FROM_/v1/models","max_tokens":128,"stream":true,"messages":[{"role":"user","content":"Reply with one short sentence."}]}'
```

网关的 debug 日志只记录模型、事件数量、响应类型和终止状态，不记录访问令牌或消息
正文。Anthropic SSE 在上游较慢时会发送 keep-alive 注释；客户端仍应等待最终的
`message_stop`，而不是把 keep-alive 当作模型内容。

Claude Code 只接受它自身已知的模型别名（例如 `sonnet`），而 Kiro 模型目录通常返回
不同的模型 ID。可显式配置别名映射；网关仍会校验映射目标必须存在于当前 Kiro 模型
目录：

```sh
export KIRO_MODEL_ALIASES='claude-sonnet-5=gpt-5.6-sol'
# 或使用动态目录中的第一个模型（适合本机冒烟测试）
export KIRO_MODEL_ALIASES='claude-sonnet-5=@first'
claude --model sonnet
```

别名只影响请求转发，响应中的模型字段使用解析后的 Kiro 模型 ID。未配置别名时仍
保持严格的精确模型校验。

### Codex、Claude Code、Grok Build 本地真实冒烟测试

仓库提供 [`scripts/test-local-clients.sh`](scripts/test-local-clients.sh)，用于在本机
启动一个隔离的网关实例，并依次检查鉴权、模型发现、三套协议的普通响应和 SSE 终止事件，
然后调用本机安装的 Codex、Claude Code 和 Grok Build。脚本默认使用 `/v1/models` 返回的
第一个模型，不把模型 ID 写死；真实上游测试必须显式确认：

```sh
export KIRO_ALLOW_LIVE_TESTS=1
```

使用环境变量凭据：

```sh
export KIRO_CREDENTIAL_SOURCE=env
export KIRO_ACCESS_TOKEN='access-token-from-your-provider'
./scripts/test-local-clients.sh
```

也可以只读本机 Kiro CLI SQLite 凭据：

```sh
export KIRO_CREDENTIAL_SOURCE=sqlite
export KIRO_CREDENTIAL_PATH="$HOME/.local/share/kiro-cli/data.sqlite3"
./scripts/test-local-clients.sh
```

脚本不会修改用户级 Codex、Claude Code 或 Grok 配置，而是在临时目录生成一次性配置；
测试结束后会停止自己启动的网关并删除临时目录。网关客户端密钥由脚本随机生成，不会
写入仓库或输出到测试日志。若要使用已经运行的网关，必须显式提供客户端密钥和网关
根地址（不要把 `/v1` 重复拼入根地址）：

```sh
export KIRO_TEST_CLIENT_API_KEY='client-key-for-the-running-gateway'
KIRO_ALLOW_LIVE_TESTS=1 \
  ./scripts/test-local-clients.sh --gateway-url http://127.0.0.1:8990
```

客户端与协议映射如下：

| 客户端 | 本地配置方式 | 验证协议 |
| --- | --- | --- |
| Codex CLI | 临时 `CODEX_HOME`，自定义 Responses provider | OpenAI Responses |
| Claude Code | `ANTHROPIC_BASE_URL`、`ANTHROPIC_API_KEY` | Anthropic Messages |
| Grok Build | 临时 `GROK_HOME`，`api_backend = "chat_completions"` | OpenAI Chat Completions |

脚本运行 Claude Code 时使用 `--setting-sources ''` 并清空代理环境，避免用户级设置
把本机请求转发到外部代理。Codex 发送的命名空间工具和 custom grammar 工具会由
Responses 适配层展开；Kiro CLI 上游不接受工具名中的点号，因此转发时将
`functions.wait` 这类名称编码为 `functions_wait`，响应返回时恢复原始命名空间。

可用 `KIRO_TEST_CLIENTS=codex,claude` 或 `--client grok` 缩小范围；可用
`KIRO_TEST_MODEL=MODEL_ID` 覆盖动态选择的模型。Grok Build 的 Responses 兼容性由脚本
中的直接 `/v1/responses` 流程检查；当前 Grok 自定义模型配置使用 Chat Completions，
不要据此假设 Grok CLI 会发送 Responses 请求。客户端冒烟提示只要求返回一句话，禁止
调用工具或修改文件。

## 配置

配置文件为 JSON。只接受下表列出的 snake_case 字段；未知字段和历史别名会直接拒绝启动。

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `host` | `127.0.0.1` | 监听地址 |
| `port` | `8990` | 监听端口，不能为 `0` |
| `client_api_key` | 空 | `/v1/*` 客户端密钥，必填 |
| `admin_api_key` | 空 | Admin 登录密钥；`admin.enabled=true` 时必填 |
| `admin.enabled` | `false` | 为 `false` 时不注册 `/admin` 页面及 `/admin/*`、`/api/admin/*` 路由；`--generate-config` 生成的配置会显式设为 `true` |
| `admin.session_ttl_secs` | `28800` | 内存会话有效期，秒 |
| `admin.cookie_secure` | `true` | 是否为 Admin Cookie 添加 `Secure` |
| `admin.allowed_origins` | `[]` | 非空时检查带 `Origin` 的 Admin 请求 |
| `admin.login_rate_limit_per_minute` | `10` | 每个登录键每分钟允许的尝试数 |
| `credential_source` | `auto` | `auto`、`env`、`json`、`sqlite`、`api_key` 之一 |
| `credential_path` | 空 | SQLite 凭据路径；支持 `~/` 展开 |
| `credential_json_path` | 空 | JSON 凭据路径及刷新后的原子写回路径；支持 `~/` 展开 |
| `endpoint` | `auto` | 上游格式；`auto` 按凭据选择，或显式指定 `ide` / `cli` |
| `api_region` | `us-east-1` | API Key 来源使用的默认区域 |
| `upstream_url` | 空 | 覆盖模型生成上游 URL，主要用于受控代理或测试 |
| `proxy_url` | 空 | Token 刷新 HTTP 客户端的代理 URL |
| `token_endpoint` | 空 | 覆盖 Token 刷新 URL，只能使用 `http`/`https` |
| `upstream_timeout_secs` | `60` | 模型上游请求超时，必须大于 `0` |
| `refresh_early_secs` | `120` | 到期前提前刷新的秒数 |
| `refresh_interval_secs` | `30` | 后台检查刷新间隔，必须大于 `0` |
| `model_cache_ttl_secs` | `300` | Kiro 模型目录缓存时间，秒，必须大于 `0` |
| `model_aliases` | `{}` | 客户端模型别名到 Kiro 模型 ID 的映射；目标也可为 `@first` |
| `response_store_path` | `kiro-gateway.sqlite3` | Responses 本地 SQLite 路径；支持 `~/` 展开 |
| `max_request_body_bytes` | `8388608` | 请求体上限（8 MiB），必须大于 `0` |
| `max_upstream_body_bytes` | `16777216` | Kiro JSON/EventStream 响应体上限（16 MiB），必须大于 `0` |
| `graceful_shutdown_timeout_secs` | `30` | SIGTERM/CTRL-C 后等待活动流的最长时间 |
| `mcp_region` | 空 | 保留字段，当前未参与运行时行为 |
| `log_json` | `false` | 使用 JSON 格式输出 tracing 日志 |
| `trust_forwarded_headers` | `false` | 为 `true` 时使用 `X-Forwarded-For` 作为 Admin 登录限流键；仅应在可信反向代理后启用 |

`config.example.json` 适合本机 HTTP 调试，因此将 `admin.cookie_secure` 设为 `false`。生产环境通过 HTTPS 使用时应设为 `true`。

### 环境变量

下列变量会覆盖对应配置或控制进程：

| 环境变量 | 用途 |
| --- | --- |
| `KIRO_CONFIG` | 配置文件路径，等价于 `--config` |
| `KIRO_CLIENT_API_KEY` | 覆盖 `client_api_key` |
| `KIRO_ADMIN_API_KEY` | 覆盖 `admin_api_key` |
| `KIRO_HOST` | 覆盖 `host` |
| `KIRO_PORT` | 覆盖 `port` |
| `KIRO_ENDPOINT` | 覆盖 `endpoint`；支持 `auto`、`ide`、`cli` |
| `KIRO_API_REGION` | 覆盖 `api_region`，也用于环境变量凭据 |
| `KIRO_CREDENTIAL_SOURCE` | 覆盖 `credential_source` |
| `KIRO_CREDENTIAL_PATH` | 覆盖 SQLite `credential_path` |
| `KIRO_CREDENTIAL_JSON_PATH` | 覆盖 JSON `credential_json_path` |
| `KIRO_UPSTREAM_URL` | 覆盖模型上游 URL |
| `KIRO_PROXY_URL` | 覆盖刷新客户端代理 URL |
| `KIRO_TOKEN_ENDPOINT` | 覆盖 `token_endpoint`，只接受 `http`/`https` |
| `KIRO_UPSTREAM_TIMEOUT_SECS` | 覆盖上游请求和刷新超时，必须大于 `0` |
| `KIRO_REFRESH_EARLY_SECS` | 覆盖提前刷新秒数，不得为负 |
| `KIRO_REFRESH_INTERVAL_SECS` | 覆盖后台刷新间隔，必须大于 `0` |
| `KIRO_MODEL_CACHE_TTL_SECS` | 覆盖模型目录缓存 TTL，必须大于 `0` |
| `KIRO_MODEL_ALIASES` | 覆盖模型别名，格式为逗号分隔的 `alias=target`；`@first` 表示当前目录第一个模型 |
| `KIRO_MAX_REQUEST_BODY_BYTES` | 覆盖请求体上限，必须大于 `0` |
| `KIRO_MAX_UPSTREAM_BODY_BYTES` | 覆盖上游响应体上限，必须大于 `0` |
| `KIRO_GRACEFUL_SHUTDOWN_TIMEOUT_SECS` | 覆盖优雅停机等待时间，必须大于 `0` |
| `KIRO_ADMIN_ENABLED` | 启用或禁用 Admin 页面和 API |
| `KIRO_ADMIN_SESSION_TTL_SECS` | 覆盖 Admin 会话 TTL，必须大于 `0` |
| `KIRO_ADMIN_COOKIE_SECURE` | 是否设置 Admin Cookie 的 `Secure` 属性 |
| `KIRO_ADMIN_LOGIN_RATE_LIMIT_PER_MINUTE` | 覆盖登录限流值，必须大于 `0` |
| `KIRO_MCP_REGION` | 覆盖保留的 MCP 区域字段 |
| `KIRO_LOG_JSON` | 覆盖保留的 JSON 日志字段 |
| `KIRO_TRUST_FORWARDED_HEADERS` | 覆盖转发头信任字段 |
| `KIRO_RESPONSE_STORE_PATH` | 覆盖 `response_store_path` |
| `RUST_LOG` | tracing 过滤器，默认 `info` |

凭据环境变量：

| 环境变量 | 用途 |
| --- | --- |
| `KIRO_ACCESS_TOKEN` | 上游访问 Token |
| `KIRO_REFRESH_TOKEN` | 上游刷新 Token |
| `KIRO_API_KEY` | 上游 API Key |
| `KIRO_CLIENT_ID` | OIDC 客户端 ID |
| `KIRO_CLIENT_SECRET` | OIDC 客户端 Secret |
| `KIRO_MACHINE_ID` | 上游机器 ID；未设置时生成随机 UUID |
| `KIRO_TOKEN_ENDPOINT` | 覆盖 Token 刷新 URL |

`KIRO_CLIENT_API_KEY` 与上游 `KIRO_API_KEY` 必须分别配置；上游密钥不会自动成为网关客户端密钥。

## 凭据来源与刷新

`credential_source` 的行为：

- `env`：从 `KIRO_ACCESS_TOKEN`、`KIRO_REFRESH_TOKEN` 或 `KIRO_API_KEY` 构造一个凭据。
- `api_key`：只读取 `KIRO_API_KEY`，并使用配置中的区域和端点。
- `json`：读取 `credential_json_path`。只接受单个凭据对象和规范 snake_case 字段：`auth_method`、`access_token`、`refresh_token`、`api_key`、`client_id`、`client_secret` 及其他文档字段；数组、camelCase、`token` 等未定义字段会拒绝。
- `sqlite`：以只读模式打开 `credential_path`，读取受支持的 Kiro Token、设备注册和 profile 元数据。
- `auto`：依次检查环境变量、已配置且存在的 JSON、已配置或默认位置的 SQLite。没有候选或发现多个候选都会拒绝启动，必须明确选择来源。

默认 SQLite 探测路径为 `~/.local/share/kiro-cli/data.sqlite3`。SQLite 始终只读，不会被网关修改。

`endpoint=auto` 是默认策略：SQLite/Kiro CLI 凭据使用 CLI 上游协议
(`runtime.{region}.kiro.dev/generateAssistantResponse`，请求目标为
`KiroRuntimeService.GenerateAssistantResponse`)，IDE/桌面凭据使用 IDE 上游协议
(`q.{region}.amazonaws.com`)。显式设置 `endpoint=cli` 或 `endpoint=ide` 会覆盖凭据元数据。这样可以避免
将只支持 CLI 应用的订阅误发到 IDE 端点而收到
`Your subscription does not support this application`。网关不会因为 IDE 返回 403 而隐式
切换到另一个端点；如果需要固定协议，请显式配置端点。

非 API Key 凭据会在启动、每次模型请求以及后台定时任务中检查是否需要刷新。刷新使用单飞锁，成功后更新内存凭据。只要配置了 `credential_json_path`，刷新后的完整凭据就会以临时文件加重命名的方式原子写回该路径；不希望落盘时不要配置该字段。应将凭据 JSON 权限限制为仅服务用户可读写。

规范 `auth_method` 值为 `api_key`、`social`、`oidc` 和 `refresh_token`；`unknown` 只用于内部解析状态，不能出现在外部凭据文件中。SQLite 中 Kiro CLI 历史 key 名（包括 `odic`）仅作为表 schema 读取，加载后的凭据统一为 `oidc`。

### SQLite 凭据验证

SQLite 凭据测试使用接近 Kiro CLI 实际 schema 的临时脱敏数据库，对合成 token、device
registration、profile 和 region 做完整断言；不会读取或提交用户的真实凭据。运行普通测试即可执行：

```sh
cargo test --locked auth::sqlite
```

如需在本机验证真实 Kiro CLI 数据库，测试默认被忽略，并且只读
`KIRO_REAL_SQLITE_PATH` 指定的文件；未设置时使用
`~/.local/share/kiro-cli/data.sqlite3`。测试只断言凭据元数据和文件未被修改，不会比较或输出
真实 token、client secret 等敏感值：

```sh
cargo test --locked auth::sqlite::tests::reads_real_kiro_cli_database_read_only -- --ignored
```

CI 不需要挂载真实 `data.sqlite3`；普通 fixture 测试已经覆盖真实表结构和字段格式。

### 模型目录与模型校验

`data.sqlite3` 只提供访问 Kiro 的凭据和 profile 元数据，不保存可用模型目录。首次访问
`GET /v1/models` 或首次发送带 `model` 的请求时，网关使用当前凭据调用 Kiro
`ListAvailableModels` 接口，并将成功结果缓存 `model_cache_ttl_secs` 秒（默认 300 秒）。
`sso_region` 为 `eu-*` 时优先使用 `eu-central-1`，其他区域优先使用 `us-east-1`；收到
403 时会尝试另一个区域端点。

`/v1/models` 返回 Kiro 返回的全部模型及可用的名称、描述和 token limits 元数据。远程
请求失败时仍返回 HTTP 200，但 `data` 为空；不会回退到静态 `kiro`。Messages、
Chat Completions、Responses 和 `count_tokens` 请求中的 `model` 必须存在于当前模型
目录，否则返回 `400`，且不会调用 Kiro 上游。若配置了 `model_aliases`，先解析别名，
再校验并转发目标模型；未配置别名时请求中的模型 ID 会原样严格校验。

如需在本机使用真实 SQLite 凭据验证完整远程链路，可执行以下默认忽略的测试；它不会
输出 token，也不会修改数据库：

```sh
cargo test --locked model_catalog::tests::discovers_models_from_real_kiro_sqlite_credentials -- --ignored
```

## Tool Call

网关保留工具调用 ID，并将当前一轮 assistant 工具调用与工具结果配对发送给 Kiro。并行调用应为每个结果使用对应 ID。更早的工具轮次会转换成可读历史文本。

### Anthropic Messages

首次请求：

```json
{
  "model": "kiro",
  "max_tokens": 1024,
  "tools": [
    {
      "name": "get_weather",
      "description": "Get weather for a city",
      "input_schema": {
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"]
      }
    }
  ],
  "messages": [
    { "role": "user", "content": "What is the weather in Paris?" }
  ]
}
```

工具调用通过 `content` 中的 `tool_use` 返回。执行工具后，把原 assistant 内容和结果一起续传：

```json
{
  "model": "kiro",
  "max_tokens": 1024,
  "tools": [
    {
      "name": "get_weather",
      "input_schema": { "type": "object" }
    }
  ],
  "messages": [
    { "role": "user", "content": "What is the weather in Paris?" },
    {
      "role": "assistant",
      "content": [
        {
          "type": "tool_use",
          "id": "call_weather_1",
          "name": "get_weather",
          "input": { "city": "Paris" }
        }
      ]
    },
    {
      "role": "user",
      "content": [
        {
          "type": "tool_result",
          "tool_use_id": "call_weather_1",
          "content": "18 C and clear"
        }
      ]
    }
  ]
}
```

流式工具调用按 `content_block_start`、`input_json_delta`、`content_block_stop` 输出，最终 `stop_reason` 为 `tool_use`。

### OpenAI Chat Completions

首次请求的 `tools` 使用 Chat Completions function 格式。收到 `assistant.tool_calls` 后，原样保留 assistant 消息，并追加对应的 `tool` 消息：

```json
{
  "model": "kiro",
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Get weather for a city",
        "parameters": {
          "type": "object",
          "properties": { "city": { "type": "string" } },
          "required": ["city"]
        }
      }
    }
  ],
  "messages": [
    { "role": "user", "content": "What is the weather in Paris?" },
    {
      "role": "assistant",
      "content": null,
      "tool_calls": [
        {
          "id": "call_weather_1",
          "type": "function",
          "function": {
            "name": "get_weather",
            "arguments": "{\"city\":\"Paris\"}"
          }
        }
      ]
    },
    {
      "role": "tool",
      "tool_call_id": "call_weather_1",
      "content": "18 C and clear"
    }
  ]
}
```

流式调用位于 `choices[0].delta.tool_calls`，每个调用包含稳定的 `index`。结束块使用 `finish_reason: "tool_calls"`，之后发送 `[DONE]`。

### OpenAI Responses

首次请求：

```json
{
  "model": "kiro",
  "store": true,
  "tools": [
    {
      "type": "function",
      "name": "get_weather",
      "description": "Get weather for a city",
      "parameters": {
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"]
      }
    }
  ],
  "input": "What is the weather in Paris?"
}
```

响应 `output` 中的 `function_call.id` 是输出 item ID，`function_call.call_id` 用于提交结果。续传时使用 `call_id`：

```json
{
  "model": "kiro",
  "previous_response_id": "resp_previous",
  "tools": [
    {
      "type": "function",
      "name": "get_weather",
      "parameters": { "type": "object" }
    }
  ],
  "input": [
    {
      "type": "function_call_output",
      "call_id": "call_weather_1",
      "output": "18 C and clear"
    }
  ]
}
```

`store` 默认为 `true`。存储开启时，工具调用会和消息一起保存，后续 `previous_response_id` 可以恢复调用上下文。`store: false` 不创建 `responses` 或 `response_events` 记录；该响应 ID 不能用于本地读取或可靠续传。带有不存在的 `previous_response_id` 会返回 `404`，不会静默当作空历史。

Responses SSE 生命周期包含 `response.created`、`response.in_progress`，随后为每个调用发送 `response.output_item.added`、工具参数增量/完成事件、`response.output_item.done`，最后发送
`response.completed` 或 `response.incomplete`。普通函数工具使用
`response.function_call_arguments.*`，custom tool 使用
`response.custom_tool_call_input.*`。所有事件包含递增的 `sequence_number`；工具事件包含
`output_index` 和独立的 `item_id`。

## Admin

`GET /admin` 提供内置管理页。Admin API 同时挂载在 `/admin/*` 和 `/api/admin/*`；以下以 `/api/admin` 为例。

登录：

```sh
curl -i -c admin.cookies \
  -H 'content-type: application/json' \
  -d '{"api_key":"admin-key-change-me"}' \
  http://127.0.0.1:8990/api/admin/auth/login
```

登录成功返回 `204` 和 `kiro_admin_session` Cookie。Cookie 为 `HttpOnly; SameSite=Lax`，并根据 `admin.cookie_secure` 添加 `Secure`。会话仅保存在进程内存，重启后失效。

读取会话并取得 CSRF Token：

```sh
curl -b admin.cookies http://127.0.0.1:8990/api/admin/auth/session
```

所有非 GET/HEAD/OPTIONS 的已登录 Admin 请求必须携带返回的 `x-csrf-token`：

```sh
export CSRF_TOKEN='csrf-value-from-session'
curl -b admin.cookies \
  -H "x-csrf-token: $CSRF_TOKEN" \
  -X POST \
  http://127.0.0.1:8990/api/admin/credential/refresh
```

Admin 路由：

| 方法 | 路径后缀 | 说明 |
| --- | --- | --- |
| `POST` | `/auth/login` | API Key 登录，无需已有会话 |
| `POST` | `/auth/logout` | 删除当前会话并清除 Cookie |
| `GET` | `/auth/session` | 会话状态与 CSRF Token |
| `GET` | `/health` | 服务健康状态 |
| `GET` | `/provider_health` | 凭据与刷新健康元数据 |
| `GET` | `/credential` | 凭据状态和刷新状态，不返回 Token |
| `POST` | `/credential/reload` | 从已配置来源重新加载单个凭据 |
| `POST` | `/credential/refresh` | 立即检查并刷新凭据 |
| `GET` | `/responses/{id}` | 读取完整的本地 Response 记录 |
| `GET` | `/responses/{id}/events` | 按 `sequence_number` 读取 Response 生命周期事件 |
| `DELETE` | `/responses/{id}` | 删除本地 Response 记录 |

当 `admin.allowed_origins` 非空时，带 `Origin` 的登录和 Admin 请求必须精确匹配列表中的一个值。默认所有 Admin 登录共享一个内存限流键；只有显式启用 `trust_forwarded_headers` 时才使用 `x-forwarded-for`，因此不要在不可信客户端可直接访问时开启该选项。

## 数据与安全边界

- 默认只监听回环地址。对外提供服务时应在可信反向代理后终止 TLS，并限制网络访问。
- `client_api_key` 与 `admin_api_key` 作用不同，应使用独立的高熵值。
- 凭据 Token 的 Debug 和普通序列化会显示为 `[REDACTED]`；健康接口和凭据状态接口只返回元数据和布尔状态。
- SQLite 凭据源只读。只有显式配置的 `credential_json_path` 会在刷新成功后写入完整凭据。
- `response_store_path` 会以明文保存请求消息、模型输出、工具参数和工具结果。应限制数据库文件权限，并按数据保留要求删除记录。
- Admin 的 `/responses/{id}` 会返回完整存储内容，只应向受信任管理员开放。
- 删除 Response 会通过外键级联删除对应的 `response_events`；事件和 Response 内容均以明文保存在本地 SQLite。
- `upstream_url` 和 `KIRO_TOKEN_ENDPOINT` 会改变凭据或内容发送目标，只能配置为受信任地址。
- 本服务不提供 TLS、跨进程会话共享或静态凭据加密。

## 构建与验证

```sh
node --check admin-ui/src/build.mjs
corepack pnpm@9.15.4 --dir admin-ui install --frozen-lockfile
corepack pnpm@9.15.4 --dir admin-ui build
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo package --locked --allow-dirty
SOURCE_DATE_EPOCH=0 ./scripts/verify-reproducible.sh
cargo audit
cargo deny --locked check
docker build -t kiro-gateway:local .
```

也可使用：

```sh
make check
make test
make reproducible
make release
```

构建固定 Rust 1.85.1。可复现构建脚本固定 `SOURCE_DATE_EPOCH`、关闭增量编译、重映射工作区路径，执行两次 release 构建并比较 SHA-256，同时检查二进制中是否残留本机绝对路径。
