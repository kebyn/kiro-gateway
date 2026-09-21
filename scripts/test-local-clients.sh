#!/usr/bin/env bash
set -Eeuo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
CLIENTS=${KIRO_TEST_CLIENTS:-all}
BASE_URL=${KIRO_GATEWAY_URL:-}
MODEL=${KIRO_TEST_MODEL:-}
PORT=${KIRO_TEST_PORT:-8990}
CLIENT_TIMEOUT=${KIRO_TEST_CLIENT_TIMEOUT:-120}
CURL_TIMEOUT=${KIRO_TEST_CURL_TIMEOUT:-120}
KEEP_GATEWAY=${KIRO_KEEP_GATEWAY:-0}
MANAGE_GATEWAY=1
CLIENT_API_KEY=${KIRO_TEST_CLIENT_API_KEY:-}
GATEWAY_PID=
PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

usage() {
    cat <<'EOF'
Usage: scripts/test-local-clients.sh [options]

Runs safe, live smoke tests against a local kiro-gateway and the installed
Codex, Claude Code, and Grok CLIs. Live testing is opt-in:

  KIRO_ALLOW_LIVE_TESTS=1 scripts/test-local-clients.sh

Options:
  --client NAME       all, codex, claude, or grok (default: all)
  --gateway-url URL   use an existing gateway and do not start one
  --model MODEL       use this model instead of the first /v1/models entry
  --keep-gateway      keep the managed gateway and temporary diagnostics
  -h, --help          show this help

Environment:
  KIRO_CREDENTIAL_SOURCE / KIRO_CREDENTIAL_PATH / KIRO_CREDENTIAL_JSON_PATH
                      select the upstream credential source
  KIRO_MODEL_ALIASES   comma-separated client-model mappings, for example
                      claude-sonnet-5=@first
  KIRO_ACCESS_TOKEN, KIRO_REFRESH_TOKEN, KIRO_API_KEY
                      provide environment credentials when the source is env
  KIRO_GATEWAY_BIN    optional built gateway binary
  CODEX_BIN, CLAUDE_BIN, GROK_BIN
                      override client executable names
  KIRO_CLAUDE_MODEL   Claude model argument (default: sonnet)
EOF
}

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

mark_pass() {
    PASS_COUNT=$((PASS_COUNT + 1))
    printf 'PASS  %s\n' "$1"
}

mark_fail() {
    FAIL_COUNT=$((FAIL_COUNT + 1))
    printf 'FAIL  %s\n' "$1"
}

mark_skip() {
    SKIP_COUNT=$((SKIP_COUNT + 1))
    printf 'SKIP  %s\n' "$1"
}

selected() {
    local name=$1
    [[ "$CLIENTS" == "all" || ",$CLIENTS," == *",$name,"* ]]
}

run_capture() {
    local stdout_path=$1
    local stderr_path=$2
    shift 2
    if command -v timeout >/dev/null 2>&1; then
        timeout --foreground "${CLIENT_TIMEOUT}s" "$@" >"$stdout_path" 2>"$stderr_path"
    else
        "$@" >"$stdout_path" 2>"$stderr_path"
    fi
}

cleanup() {
    if [[ -n "$GATEWAY_PID" ]] && kill -0 "$GATEWAY_PID" 2>/dev/null; then
        kill "$GATEWAY_PID" 2>/dev/null || true
        wait "$GATEWAY_PID" 2>/dev/null || true
    fi
    if [[ "${KEEP_GATEWAY}" == "1" ]]; then
        printf 'Diagnostics kept at %s\n' "$TMP_DIR" >&2
    else
        for attempt in 1 2 3 4 5; do
            if rm -rf "$TMP_DIR"; then
                return
            fi
            sleep 1
        done
        printf 'WARNING: could not remove temporary diagnostics at %s\n' "$TMP_DIR" >&2
    fi
}

while (($# > 0)); do
    case "$1" in
        --client)
            (($# >= 2)) || die "--client requires a value"
            CLIENTS=$2
            shift 2
            ;;
        --gateway-url)
            (($# >= 2)) || die "--gateway-url requires a value"
            BASE_URL=${2%/}
            MANAGE_GATEWAY=0
            shift 2
            ;;
        --model)
            (($# >= 2)) || die "--model requires a value"
            MODEL=$2
            shift 2
            ;;
        --keep-gateway)
            KEEP_GATEWAY=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

case ",$CLIENTS," in
    *,all,*|*,codex,*|*,claude,*|*,grok,*) ;;
    *) die "unsupported client selection: $CLIENTS" ;;
esac

[[ "${KIRO_ALLOW_LIVE_TESTS:-0}" == "1" ]] ||
    die "live testing is disabled; set KIRO_ALLOW_LIVE_TESTS=1 explicitly"

for command_name in curl jq; do
    command -v "$command_name" >/dev/null 2>&1 ||
        die "required command is missing: $command_name"
done

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/kiro-gateway-client-test.XXXXXX")
chmod 700 "$TMP_DIR"
trap cleanup EXIT INT TERM

if [[ -z "$CLIENT_API_KEY" && "$MANAGE_GATEWAY" == "1" ]]; then
    if command -v openssl >/dev/null 2>&1; then
        CLIENT_API_KEY=$(openssl rand -hex 32)
    else
        CLIENT_API_KEY=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
    fi
fi

if [[ "$MANAGE_GATEWAY" == "0" && -z "$CLIENT_API_KEY" ]]; then
    die "KIRO_TEST_CLIENT_API_KEY is required with --gateway-url"
fi

CREDENTIAL_SOURCE=${KIRO_TEST_CREDENTIAL_SOURCE:-${KIRO_CREDENTIAL_SOURCE:-auto}}
if [[ "$CREDENTIAL_SOURCE" == "env" ]] &&
    [[ -z "${KIRO_ACCESS_TOKEN:-}" && -z "${KIRO_REFRESH_TOKEN:-}" && -z "${KIRO_API_KEY:-}" ]]; then
    die "KIRO_CREDENTIAL_SOURCE=env requires KIRO_ACCESS_TOKEN, KIRO_REFRESH_TOKEN, or KIRO_API_KEY"
fi

for client in codex claude grok; do
    if selected "$client"; then
        command_name=${client}
        case "$client" in
            codex) command_name=${CODEX_BIN:-codex} ;;
            claude) command_name=${CLAUDE_BIN:-claude} ;;
            grok) command_name=${GROK_BIN:-grok} ;;
        esac
        if ! command -v "$command_name" >/dev/null 2>&1; then
            mark_fail "$client CLI is not installed ($command_name)"
        else
            version=$("$command_name" --version 2>&1 | head -n 1 || true)
            printf 'INFO  %s: %s\n' "$client" "${version:-version unavailable}"
        fi
    fi
done
((FAIL_COUNT == 0)) || exit 1

if [[ "$MANAGE_GATEWAY" == "1" ]]; then
    BASE_URL="http://127.0.0.1:$PORT"
    if curl --silent --show-error --max-time 2 "$BASE_URL/health" >/dev/null 2>&1; then
        die "$BASE_URL is already serving; pass --gateway-url to use it explicitly"
    fi

    GATEWAY_ENV=(
        "KIRO_HOST=127.0.0.1"
        "KIRO_PORT=$PORT"
        "KIRO_CLIENT_API_KEY=$CLIENT_API_KEY"
        "KIRO_CREDENTIAL_SOURCE=$CREDENTIAL_SOURCE"
        "KIRO_RESPONSE_STORE_PATH=$TMP_DIR/responses.sqlite3"
        "RUST_LOG=${RUST_LOG:-warn}"
    )
    if [[ -n "${KIRO_CREDENTIAL_PATH:-}" ]]; then
        GATEWAY_ENV+=("KIRO_CREDENTIAL_PATH=$KIRO_CREDENTIAL_PATH")
    fi
    if [[ -n "${KIRO_CREDENTIAL_JSON_PATH:-}" ]]; then
        GATEWAY_ENV+=("KIRO_CREDENTIAL_JSON_PATH=$KIRO_CREDENTIAL_JSON_PATH")
    fi
    if [[ -n "${KIRO_MODEL_ALIASES:-}" ]]; then
        GATEWAY_ENV+=("KIRO_MODEL_ALIASES=$KIRO_MODEL_ALIASES")
    elif selected claude; then
        GATEWAY_ENV+=("KIRO_MODEL_ALIASES=claude-sonnet-5=@first")
    fi

    if [[ -n "${KIRO_GATEWAY_BIN:-}" ]]; then
        GATEWAY_COMMAND=("$KIRO_GATEWAY_BIN")
    else
        command -v cargo >/dev/null 2>&1 || die "cargo is required when no gateway binary is available"
        GATEWAY_COMMAND=(cargo run --quiet --locked --manifest-path "$ROOT/Cargo.toml" --)
    fi

    if ! env "${GATEWAY_ENV[@]}" "${GATEWAY_COMMAND[@]}" --check-config \
        >"$TMP_DIR/check-config.out" 2>"$TMP_DIR/check-config.err"; then
        cat "$TMP_DIR/check-config.err" >&2
        die "gateway configuration check failed"
    fi

    env "${GATEWAY_ENV[@]}" "${GATEWAY_COMMAND[@]}" \
        >"$TMP_DIR/gateway.log" 2>&1 &
    GATEWAY_PID=$!
    ready=0
    for _ in $(seq 1 120); do
        if curl --silent --show-error --fail --max-time 2 "$BASE_URL/health" >/dev/null 2>&1; then
            ready=1
            break
        fi
        if ! kill -0 "$GATEWAY_PID" 2>/dev/null; then
            tail -n 40 "$TMP_DIR/gateway.log" >&2 || true
            die "gateway exited before becoming healthy"
        fi
        sleep 0.5
    done
    ((ready == 1)) || die "gateway did not become healthy within 60 seconds"
fi

auth_header=(-H "x-api-key: $CLIENT_API_KEY")
json_header=(-H "content-type: application/json" -H "accept: application/json")
sse_header=(-H "content-type: application/json" -H "accept: text/event-stream")

health_body="$TMP_DIR/health.json"
if curl --silent --show-error --fail --max-time "$CURL_TIMEOUT" \
    "$BASE_URL/health" >"$health_body" &&
    jq -e '.ok == true and .service == "kiro-gateway"' "$health_body" >/dev/null; then
    mark_pass "gateway health"
else
    mark_fail "gateway health"
fi

unauth_status=$(curl --silent --output /dev/null --write-out '%{http_code}' \
    --max-time "$CURL_TIMEOUT" "$BASE_URL/v1/models" || true)
if [[ "$unauth_status" == "401" ]]; then
    mark_pass "client API key enforcement"
else
    mark_fail "client API key enforcement (expected 401, got $unauth_status)"
fi

models_body="$TMP_DIR/models.json"
if curl --silent --show-error --fail --max-time "$CURL_TIMEOUT" \
    "${auth_header[@]}" "$BASE_URL/v1/models" >"$models_body" &&
    jq -e '.object == "list" and (.data | type == "array") and (.data | length > 0)' \
        "$models_body" >/dev/null; then
    if [[ -z "$MODEL" ]]; then
        MODEL=$(jq -er '.data[0].id' "$models_body")
    fi
    printf 'INFO  model: %s\n' "$MODEL"
    mark_pass "model discovery"
else
    mark_fail "model discovery"
    exit 1
fi

anthropic_payload=$(jq -nc --arg model "$MODEL" \
    '{model:$model,max_tokens:64,stream:false,messages:[{role:"user",content:"Reply with exactly one short sentence."}]}')
if printf '%s' "$anthropic_payload" |
    curl --silent --show-error --fail --max-time "$CURL_TIMEOUT" \
        "${auth_header[@]}" "${json_header[@]}" "$BASE_URL/v1/messages" \
        --data-binary @- >"$TMP_DIR/anthropic.json" &&
    jq -e '.type == "message" and (.content | type == "array")' "$TMP_DIR/anthropic.json" >/dev/null; then
    mark_pass "Anthropic Messages text response"
else
    mark_fail "Anthropic Messages text response"
fi

anthropic_stream_payload=$(jq -nc --arg model "$MODEL" \
    '{model:$model,max_tokens:64,stream:true,messages:[{role:"user",content:"Reply with exactly one short sentence."}]}')
if printf '%s' "$anthropic_stream_payload" |
    curl --silent --show-error --fail --max-time "$CURL_TIMEOUT" \
        "${auth_header[@]}" "${sse_header[@]}" "$BASE_URL/v1/messages" \
        --data-binary @- >"$TMP_DIR/anthropic.sse" &&
    grep -q 'event: message_stop' "$TMP_DIR/anthropic.sse"; then
    mark_pass "Anthropic Messages SSE termination"
else
    mark_fail "Anthropic Messages SSE termination"
fi

chat_payload=$(jq -nc --arg model "$MODEL" \
    '{model:$model,stream:false,messages:[{role:"user",content:"Reply with exactly one short sentence."}]}')
if printf '%s' "$chat_payload" |
    curl --silent --show-error --fail --max-time "$CURL_TIMEOUT" \
        "${auth_header[@]}" "${json_header[@]}" "$BASE_URL/v1/chat/completions" \
        --data-binary @- >"$TMP_DIR/chat.json" &&
    jq -e '.object == "chat.completion" and (.choices | length > 0)' "$TMP_DIR/chat.json" >/dev/null; then
    mark_pass "OpenAI Chat Completions text response"
else
    mark_fail "OpenAI Chat Completions text response"
fi

chat_stream_payload=$(jq -nc --arg model "$MODEL" \
    '{model:$model,stream:true,messages:[{role:"user",content:"Reply with exactly one short sentence."}]}')
if printf '%s' "$chat_stream_payload" |
    curl --silent --show-error --fail --max-time "$CURL_TIMEOUT" \
        "${auth_header[@]}" "${sse_header[@]}" "$BASE_URL/v1/chat/completions" \
        --data-binary @- >"$TMP_DIR/chat.sse" &&
    grep -q '\[DONE\]' "$TMP_DIR/chat.sse"; then
    mark_pass "OpenAI Chat Completions SSE termination"
else
    mark_fail "OpenAI Chat Completions SSE termination"
fi

responses_payload=$(jq -nc --arg model "$MODEL" \
    '{model:$model,store:false,stream:false,input:"Reply with exactly one short sentence."}')
if printf '%s' "$responses_payload" |
    curl --silent --show-error --fail --max-time "$CURL_TIMEOUT" \
        "${auth_header[@]}" "${json_header[@]}" "$BASE_URL/v1/responses" \
        --data-binary @- >"$TMP_DIR/responses.json" &&
    jq -e '.object == "response" and (.id | type == "string")' "$TMP_DIR/responses.json" >/dev/null; then
    mark_pass "OpenAI Responses text response"
else
    mark_fail "OpenAI Responses text response"
fi

responses_stream_payload=$(jq -nc --arg model "$MODEL" \
    '{model:$model,store:false,stream:true,input:"Reply with exactly one short sentence."}')
if printf '%s' "$responses_stream_payload" |
    curl --silent --show-error --fail --max-time "$CURL_TIMEOUT" \
        "${auth_header[@]}" "${sse_header[@]}" "$BASE_URL/v1/responses" \
        --data-binary @- >"$TMP_DIR/responses.sse" &&
    grep -Eq 'response\.(completed|incomplete)' "$TMP_DIR/responses.sse"; then
    mark_pass "OpenAI Responses SSE termination"
else
    mark_fail "OpenAI Responses SSE termination"
fi

client_prompt='Reply with exactly one short sentence confirming the gateway connection. Do not use tools or modify files.'

if selected codex; then
    codex_bin=${CODEX_BIN:-codex}
    codex_home="$TMP_DIR/codex-home"
    mkdir -m 700 "$codex_home"
    cat >"$codex_home/config.toml" <<EOF
model_provider = "kiro-gateway"
model = "$MODEL"
approval_policy = "never"
sandbox_mode = "read-only"

[model_providers.kiro-gateway]
name = "kiro-gateway"
base_url = "$BASE_URL/v1"
experimental_bearer_token = "$CLIENT_API_KEY"
wire_api = "responses"
requires_openai_auth = true
request_max_retries = 0
stream_max_retries = 0
EOF
    chmod 600 "$codex_home/config.toml"
    if run_capture "$TMP_DIR/codex.out" "$TMP_DIR/codex.err" \
        env CODEX_HOME="$codex_home" "$codex_bin" exec --ephemeral \
        --skip-git-repo-check --sandbox read-only -C "$ROOT" -m "$MODEL" --json "$client_prompt"; then
        [[ -s "$TMP_DIR/codex.out" ]] &&
            mark_pass "Codex Responses client call" ||
            mark_fail "Codex Responses client call (empty output)"
    else
        mark_fail "Codex Responses client call"
    fi
fi

if selected claude; then
    claude_bin=${CLAUDE_BIN:-claude}
    claude_model=${KIRO_CLAUDE_MODEL:-sonnet}
    if run_capture "$TMP_DIR/claude.out" "$TMP_DIR/claude.err" \
        env ANTHROPIC_BASE_URL="$BASE_URL" ANTHROPIC_API_KEY="$CLIENT_API_KEY" \
        HTTP_PROXY= HTTPS_PROXY= ALL_PROXY= NO_PROXY= \
        http_proxy= https_proxy= all_proxy= no_proxy= \
        "$claude_bin" --bare --print --no-session-persistence --max-turns 1 \
        --setting-sources '' --permission-mode plan --model "$claude_model" \
        --output-format json "$client_prompt"; then
        [[ -s "$TMP_DIR/claude.out" ]] &&
            mark_pass "Claude Code Anthropic client call" ||
            mark_fail "Claude Code Anthropic client call (empty output)"
    else
        mark_fail "Claude Code Anthropic client call"
    fi
fi

if selected grok; then
    grok_bin=${GROK_BIN:-grok}
    grok_home="$TMP_DIR/grok-home"
    mkdir -m 700 "$grok_home"
    cat >"$grok_home/config.toml" <<EOF
[models]
default = "kiro-gateway"

[model.kiro-gateway]
name = "kiro-gateway"
model = "$MODEL"
base_url = "$BASE_URL/v1"
api_key = "$CLIENT_API_KEY"
api_backend = "chat_completions"
max_retries = 0
stream_tool_calls = true

[ui]
permission_mode = "plan"
EOF
    chmod 600 "$grok_home/config.toml"
    if run_capture "$TMP_DIR/grok.out" "$TMP_DIR/grok.err" \
        env GROK_HOME="$grok_home" "$grok_bin" --no-plan --no-subagents \
        --no-alt-screen --output-format json --permission-mode plan \
        --model kiro-gateway --single "$client_prompt"; then
        [[ -s "$TMP_DIR/grok.out" ]] &&
            mark_pass "Grok Build Chat Completions client call" ||
            mark_fail "Grok Build Chat Completions client call (empty output)"
    else
        mark_fail "Grok Build Chat Completions client call"
    fi
fi

printf '\nSummary: %d passed, %d failed, %d skipped\n' \
    "$PASS_COUNT" "$FAIL_COUNT" "$SKIP_COUNT"
if ((FAIL_COUNT > 0)); then
    exit 1
fi
