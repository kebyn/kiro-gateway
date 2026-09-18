# kiro-gateway-rs

单租户 Kiro 网关，提供 Anthropic Messages、OpenAI Chat Completions 和 OpenAI Responses 兼容接口。项目独立于 `/data/kiro.rs`、`/data/Kiro-Go` 与 `/data/kiro-gateway`，凭据只加载一个有效候选，不包含账号池。

## 快速开始

```sh
cp config.example.json config.json
export KIRO_CLIENT_API_KEY=sk-client-example
export KIRO_ADMIN_API_KEY=sk-admin-example
export KIRO_ACCESS_TOKEN=...
cargo run --locked -- --config config.json
```

`GET /health` 不需要鉴权。模型接口接受 `x-api-key` 或 `Authorization: Bearer`，二者都必须匹配 `client_api_key`。Admin 登录为 `POST /admin/auth/login`，请求体为 `{"api_key":"..."}`；登录成功后使用 HttpOnly 会话 Cookie，修改请求需要 `x-csrf-token`。

接口：

- `POST /v1/messages`、`POST /v1/messages/count_tokens`
- `POST /v1/chat/completions`
- `POST /v1/responses`、`GET/DELETE /v1/responses/:id`
- `GET /api/admin/credential`、`POST /api/admin/credential/reload|refresh`

## 构建与验证

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
SOURCE_DATE_EPOCH=0 ./scripts/verify-reproducible.sh
```

构建脚本固定 Rust 1.85.1、`SOURCE_DATE_EPOCH`、增量编译关闭和路径重映射；SQLite 默认只读加载，只有显式配置 JSON 路径时才会原子回写刷新后的凭据。日志和健康接口只暴露凭据元数据，不输出 Token。

