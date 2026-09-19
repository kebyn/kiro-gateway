# kiro-gateway

单租户 Kiro API 网关，提供 Anthropic Messages、OpenAI Chat Completions 和 OpenAI Responses 兼容接口。服务只选择一个凭据，不包含账号池、轮询或故障切换逻辑。

## 功能

- Anthropic Messages：普通响应、SSE、Token 估算、Tool Call 与 Tool Result 续传。
- OpenAI Chat Completions：普通响应、SSE、并行 `tool_calls` 与 `tool` 消息续传。
- OpenAI Responses：普通响应、SSE、`function_call` / `function_call_output`、本地响应存储和 `previous_response_id`。
- Kiro IDE 与 CLI 两种上游端点格式。
- 环境变量、JSON、只读 SQLite 和 API Key 凭据来源。
- 凭据自动刷新、Admin 会话与 CSRF 防护。
- 固定 Rust 工具链和可复现构建检查。

## 安装与启动

需要 Rust 1.85.1。仓库中的 `rust-toolchain.toml` 会让 rustup 自动选择该版本。

### 使用环境变量凭据

```sh
cp config.example.json config.json
chmod 600 config.json

export KIRO_CLIENT_API_KEY='client-key-change-me'
export KIRO_ADMIN_API_KEY='admin-key-change-me'
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

### Docker

```sh
docker build -t kiro-gateway .
docker run --rm -p 8990:8990 \
  -e KIRO_CLIENT_API_KEY='client-key-change-me' \
  -e KIRO_ADMIN_API_KEY='admin-key-change-me' \
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
  -e KIRO_CREDENTIAL_SOURCE='env' \
  -e KIRO_ACCESS_TOKEN='access-token-from-your-provider' \
  ghcr.io/kebyn/kiro-gateway:latest
```

推送到 `master` 会更新 `latest`；推送 Git tag 时会同时发布同名镜像 tag 和 `latest`，例如
`ghcr.io/kebyn/kiro-gateway:v1.0.0`。Pull Request 和其他分支只执行镜像构建校验，不会发布。

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

## 配置

配置文件为 JSON。未列出的字段使用下表默认值；`apiKey`、`adminApiKey`、`defaultEndpoint` 分别是 `client_api_key`、`admin_api_key`、`endpoint` 的兼容别名。

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `host` | `127.0.0.1` | 监听地址 |
| `port` | `8990` | 监听端口，不能为 `0` |
| `client_api_key` | 空 | `/v1/*` 客户端密钥，必填 |
| `admin_api_key` | 空 | Admin 登录密钥；`admin.enabled=true` 时必填 |
| `admin.enabled` | `true` | 为 `false` 时不注册 `/admin` 页面及 `/admin/*`、`/api/admin/*` 路由 |
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
| `upstream_timeout_secs` | `60` | 模型上游请求超时，必须大于 `0` |
| `refresh_early_secs` | `120` | 到期前提前刷新的秒数 |
| `refresh_interval_secs` | `30` | 后台检查刷新间隔，必须大于 `0` |
| `model_cache_ttl_secs` | `300` | Kiro 模型目录缓存时间，秒，必须大于 `0` |
| `response_store_path` | `kiro-gateway.sqlite3` | Responses 本地 SQLite 路径；支持 `~/` 展开 |
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
| `KIRO_UPSTREAM_TIMEOUT_SECS` | 覆盖上游请求和刷新超时，必须大于 `0` |
| `KIRO_REFRESH_EARLY_SECS` | 覆盖提前刷新秒数，不得为负 |
| `KIRO_REFRESH_INTERVAL_SECS` | 覆盖后台刷新间隔，必须大于 `0` |
| `KIRO_MODEL_CACHE_TTL_SECS` | 覆盖模型目录缓存 TTL，必须大于 `0` |
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
- `json`：读取 `credential_json_path`。支持 snake_case/camelCase 字段；应提供单个凭据对象。
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
目录，否则返回 `400`，且不会调用 Kiro 上游。请求中的模型 ID 会原样转发。

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

Responses SSE 生命周期包含 `response.created`、`response.in_progress`，随后为每个调用发送 `response.output_item.added`、`response.function_call_arguments.delta`、`response.function_call_arguments.done`、`response.output_item.done`，最后发送 `response.completed` 或 `response.incomplete`。所有事件包含递增的 `sequence_number`；工具事件包含 `output_index` 和独立的 `item_id`。

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
SOURCE_DATE_EPOCH=0 ./scripts/verify-reproducible.sh
```

也可使用：

```sh
make check
make test
make reproducible
make release
```

构建固定 Rust 1.85.1。可复现构建脚本固定 `SOURCE_DATE_EPOCH`、关闭增量编译、重映射工作区路径，执行两次 release 构建并比较 SHA-256，同时检查二进制中是否残留本机绝对路径。
