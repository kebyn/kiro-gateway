# kiro-gateway-rs

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
./target/release/kiro-gateway-rs --config config.json
```

`--config` 也可通过 `KIRO_CONFIG` 指定。`--check-config` 会解析并打印最终配置后退出；输出包含网关 API Key，不要写入日志或在共享终端使用。

### Docker

```sh
docker build -t kiro-gateway-rs .
docker run --rm -p 8990:8990 \
  -e KIRO_CLIENT_API_KEY='client-key-change-me' \
  -e KIRO_ADMIN_API_KEY='admin-key-change-me' \
  -e KIRO_CREDENTIAL_SOURCE='env' \
  -e KIRO_ACCESS_TOKEN='access-token-from-your-provider' \
  kiro-gateway-rs
```

镜像以 UID `10001` 的非 root 用户运行，并设置 `KIRO_HOST=0.0.0.0`。如使用 JSON/SQLite 凭据或需要保留 Responses 数据库，请挂载对应文件或目录，并确保该用户具有所需的读写权限。

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

三套生成接口均支持 `stream: true`，响应类型为 SSE。

## 配置

配置文件为 JSON。未列出的字段使用下表默认值；`apiKey`、`adminApiKey`、`defaultEndpoint` 分别是 `client_api_key`、`admin_api_key`、`endpoint` 的兼容别名。

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `host` | `127.0.0.1` | 监听地址 |
| `port` | `8990` | 监听端口，不能为 `0` |
| `client_api_key` | 空 | `/v1/*` 客户端密钥，必填 |
| `admin_api_key` | 空 | Admin 登录密钥；`admin.enabled=true` 时必填 |
| `admin.enabled` | `true` | 当前用于 Admin 配置校验；当前版本仍会注册 Admin 路由 |
| `admin.session_ttl_secs` | `28800` | 内存会话有效期，秒 |
| `admin.cookie_secure` | `true` | 是否为 Admin Cookie 添加 `Secure` |
| `admin.allowed_origins` | `[]` | 非空时检查带 `Origin` 的 Admin 请求 |
| `admin.login_rate_limit_per_minute` | `10` | 每个登录键每分钟允许的尝试数 |
| `credential_source` | `auto` | `auto`、`env`、`json`、`sqlite`、`api_key` 之一 |
| `credential_path` | 空 | SQLite 凭据路径；支持 `~/` 展开 |
| `credential_json_path` | 空 | JSON 凭据路径及刷新后的原子写回路径；支持 `~/` 展开 |
| `endpoint` | `ide` | 上游格式，`ide` 或 `cli`；其他值按 `ide` 处理 |
| `api_region` | `us-east-1` | API Key 来源使用的默认区域 |
| `upstream_url` | 空 | 覆盖模型生成上游 URL，主要用于受控代理或测试 |
| `proxy_url` | 空 | Token 刷新 HTTP 客户端的代理 URL |
| `upstream_timeout_secs` | `60` | 模型上游请求超时，必须大于 `0` |
| `refresh_early_secs` | `120` | 到期前提前刷新的秒数 |
| `refresh_interval_secs` | `30` | 后台检查刷新间隔，必须大于 `0` |
| `response_store_path` | `kiro-gateway.sqlite3` | Responses 本地 SQLite 路径；支持 `~/` 展开 |
| `mcp_region` | 空 | 保留字段，当前未参与运行时行为 |
| `log_json` | `false` | 保留字段，当前未参与日志初始化 |
| `trust_forwarded_headers` | `false` | 保留字段，当前未改变转发头处理 |

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
| `KIRO_ENDPOINT` | 覆盖 `endpoint`，也用于环境变量凭据元数据 |
| `KIRO_API_REGION` | 覆盖 `api_region`，也用于环境变量凭据 |
| `KIRO_CREDENTIAL_SOURCE` | 覆盖 `credential_source` |
| `KIRO_CREDENTIAL_PATH` | 覆盖 SQLite `credential_path` |
| `KIRO_RESPONSE_STORE_PATH` | 覆盖 `response_store_path` |
| `RUST_LOG` | tracing 过滤器，默认 `info` |

凭据环境变量：

| 环境变量 | 用途 |
| --- | --- |
| `KIRO_ACCESS_TOKEN` | 上游访问 Token |
| `KIRO_REFRESH_TOKEN` | 上游刷新 Token |
| `KIRO_API_KEY` | 上游 API Key；未设置 `KIRO_CLIENT_API_KEY` 时也会作为客户端密钥回退值 |
| `KIRO_CLIENT_ID` | OIDC 客户端 ID |
| `KIRO_CLIENT_SECRET` | OIDC 客户端 Secret |
| `KIRO_MACHINE_ID` | 上游机器 ID；未设置时生成随机 UUID |
| `KIRO_TOKEN_ENDPOINT` | 覆盖 Token 刷新 URL |

建议始终显式设置 `KIRO_CLIENT_API_KEY`，避免将上游 `KIRO_API_KEY` 同时用作网关客户端密钥。

## 凭据来源与刷新

`credential_source` 的行为：

- `env`：从 `KIRO_ACCESS_TOKEN`、`KIRO_REFRESH_TOKEN` 或 `KIRO_API_KEY` 构造一个凭据。
- `api_key`：只读取 `KIRO_API_KEY`，并使用配置中的区域和端点。
- `json`：读取 `credential_json_path`。支持 snake_case/camelCase 字段；应提供单个凭据对象。
- `sqlite`：以只读模式打开 `credential_path`，读取受支持的 Kiro Token、设备注册和 profile 元数据。
- `auto`：依次检查环境变量、已配置且存在的 JSON、已配置或默认位置的 SQLite。没有候选或发现多个候选都会拒绝启动，必须明确选择来源。

默认 SQLite 探测路径为 `~/.local/share/kiro-cli/data.sqlite3`。SQLite 始终只读，不会被网关修改。

非 API Key 凭据会在启动、每次模型请求以及后台定时任务中检查是否需要刷新。刷新使用单飞锁，成功后更新内存凭据。只要配置了 `credential_json_path`，刷新后的完整凭据就会以临时文件加重命名的方式原子写回该路径；不希望落盘时不要配置该字段。应将凭据 JSON 权限限制为仅服务用户可读写。

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

`store` 默认为 `true`。存储开启时，工具调用会和消息一起保存，后续 `previous_response_id` 可以恢复调用上下文。`store: false` 不创建本地记录，该 ID 不能用于本地读取或可靠续传。

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
| `GET` | `/request-logs` | 当前返回空列表，尚未持久化请求日志 |
| `DELETE` | `/request-logs` | 当前返回 `204` |
| `GET` | `/responses/{id}` | 读取完整的本地 Response 记录 |
| `DELETE` | `/responses/{id}` | 删除本地 Response 记录 |

当 `admin.allowed_origins` 非空时，带 `Origin` 的登录和 Admin 请求必须精确匹配列表中的一个值。登录尝试按 `x-forwarded-for` 值或全局键进行内存限流。

## 数据与安全边界

- 默认只监听回环地址。对外提供服务时应在可信反向代理后终止 TLS，并限制网络访问。
- `client_api_key` 与 `admin_api_key` 作用不同，应使用独立的高熵值。
- 凭据 Token 的 Debug 和普通序列化会显示为 `[REDACTED]`；健康接口和凭据状态接口只返回元数据和布尔状态。
- SQLite 凭据源只读。只有显式配置的 `credential_json_path` 会在刷新成功后写入完整凭据。
- `response_store_path` 会以明文保存请求消息、模型输出、工具参数和工具结果。应限制数据库文件权限，并按数据保留要求删除记录。
- Admin 的 `/responses/{id}` 会返回完整存储内容，只应向受信任管理员开放。
- `upstream_url` 和 `KIRO_TOKEN_ENDPOINT` 会改变凭据或内容发送目标，只能配置为受信任地址。
- 本服务不提供 TLS、跨进程会话共享或静态凭据加密。

## 构建与验证

```sh
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
