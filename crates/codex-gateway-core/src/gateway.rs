use crate::account::{ensure_fresh, Account};
use crate::config::GatewayKey;
use crate::store::Store;
use anyhow::{anyhow, Context, Result};
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use base64::{engine::general_purpose, Engine as _};
use chrono::Utc;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};

const DEFAULT_CODEX_USER_AGENT: &str = "codex-tui/0.118.0 (Linux; x86_64) cockpit-codex-cli/0.1.0";
const DEFAULT_CODEX_ORIGINATOR: &str = "codex-tui";
const CODEX_IMAGE_MODEL_ID: &str = "gpt-image-2";
const DEFAULT_IMAGES_MAIN_MODEL: &str = "gpt-5.4-mini";
const RESPONSES_PATH: &str = "/v1/responses";
const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const IMAGES_GENERATIONS_PATH: &str = "/v1/images/generations";
const IMAGES_EDITS_PATH: &str = "/v1/images/edits";

#[derive(Debug, Clone)]
pub struct GatewayOptions {
    pub host: String,
    pub port: u16,
    pub upstream_base_url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    pub ok: bool,
    pub status: Option<u16>,
    pub output: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
struct AppState {
    store: Store,
    gateway_key: GatewayKey,
    upstream_base_url: String,
    client: reqwest::Client,
    cursor: Arc<AtomicUsize>,
    affinity: Arc<Mutex<HashMap<String, String>>>,
}

#[derive(Debug, Clone)]
struct ParsedGatewayRequest {
    method: Method,
    target: String,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Debug, Clone)]
enum ResponseAdapter {
    Passthrough,
    ChatCompletions,
    Images { response_format: String },
}

pub async fn serve(store: Store, options: GatewayOptions) -> Result<()> {
    store.init()?;
    let gateway_key = store.gateway_key()?;
    let state = AppState {
        store,
        gateway_key,
        upstream_base_url: options.upstream_base_url.trim_end_matches('/').to_string(),
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .context("build HTTP client")?,
        cursor: Arc::new(AtomicUsize::new(0)),
        affinity: Arc::new(Mutex::new(HashMap::new())),
    };
    let app = router(state);
    let addr: SocketAddr = format!("{}:{}", options.host, options.port)
        .parse()
        .with_context(|| format!("invalid listen address {}:{}", options.host, options.port))?;
    if options.host == "0.0.0.0" {
        warn!("serving plain HTTP on 0.0.0.0; Bearer keys are visible on the network without TLS");
    }
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {}", addr))?;
    info!("cockpit-codex gateway listening on http://{}", addr);
    axum::serve(listener, app).await.context("serve gateway")
}

pub async fn test_gateway(base_url: &str, api_key: &str, model: &str) -> Result<TestResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let response = client
        .post(format!("{}/v1/responses", base_url.trim_end_matches('/')))
        .bearer_auth(api_key)
        .json(&json!({
            "model": model,
            "stream": false,
            "store": false,
            "input": "Reply with exactly: pong"
        }))
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return Ok(TestResult {
                ok: false,
                status: error.status().map(|status| status.as_u16()),
                output: None,
                error: Some(error.to_string()),
            });
        }
    };
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    Ok(TestResult {
        ok: status.is_success(),
        status: Some(status.as_u16()),
        output: status
            .is_success()
            .then(|| extract_output_text_from_response_text(&text)),
        error: (!status.is_success()).then_some(text),
    })
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/models", any(handle))
        .fallback(any(handle))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state)
}

async fn handle(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
        Ok(body) => body,
        Err(error) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!("read body failed: {}", error),
            )
        }
    };
    let parsed = ParsedGatewayRequest {
        method: parts.method,
        target: normalize_target(&parts.uri),
        headers: parts.headers,
        body,
    };

    if parsed.method == Method::OPTIONS {
        return StatusCode::NO_CONTENT.into_response();
    }
    if parsed.method != Method::GET && parsed.method != Method::POST {
        return json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only GET and POST are allowed",
        );
    }
    if !parsed.target.starts_with("/v1/") {
        return json_error(StatusCode::NOT_FOUND, "Not Found");
    }
    if !authorized(&parsed.headers, &state.gateway_key.0) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "missing or invalid gateway API key",
        );
    }
    if is_models_request(&parsed.target) {
        return json_ok(build_models_response());
    }
    let (prepared, adapter) = match prepare_gateway_request(parsed) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match dispatch_with_accounts(&state, &prepared).await {
        Ok((response, account_id)) => adapt_response(response, adapter, &state, account_id).await,
        Err(error) => json_error(error.status, error.message),
    }
}

async fn dispatch_with_accounts(
    state: &AppState,
    request: &ParsedGatewayRequest,
) -> Result<(reqwest::Response, String), DispatchError> {
    let mut accounts = state
        .store
        .list_accounts()
        .map_err(DispatchError::unavailable)?;
    if accounts.is_empty() {
        return Err(DispatchError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no accounts imported",
        ));
    }
    let previous = previous_response_id(&request.body);
    if let Some(previous) = previous {
        if let Some(account_id) = state.affinity.lock().await.get(&previous).cloned() {
            accounts.sort_by_key(|account| if account.id == account_id { 0 } else { 1 });
        }
    } else {
        let start = state.cursor.fetch_add(1, Ordering::Relaxed);
        let account_count = accounts.len();
        accounts.rotate_left(start % account_count);
    }

    let mut last_error = DispatchError::new(StatusCode::SERVICE_UNAVAILABLE, "no usable account");
    for mut account in accounts {
        if let Err(error) = ensure_fresh_and_save(state, &mut account).await {
            last_error = DispatchError::new(
                StatusCode::UNAUTHORIZED,
                format!("account {} refresh failed: {}", account.email, error),
            );
            continue;
        }
        match send_upstream(state, request, &account).await {
            Ok(response) if response.status().is_success() => return Ok((response, account.id)),
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                if let Err(error) = force_refresh_and_save(state, &mut account).await {
                    last_error = DispatchError::new(
                        StatusCode::UNAUTHORIZED,
                        format!("account {} refresh failed: {}", account.email, error),
                    );
                    continue;
                }
                match send_upstream(state, request, &account).await {
                    Ok(response) if response.status().is_success() => {
                        return Ok((response, account.id))
                    }
                    Ok(response) => {
                        last_error = upstream_error(response).await;
                    }
                    Err(error) => last_error = DispatchError::bad_gateway(error),
                }
            }
            Ok(response) => {
                let should_next = should_try_next_account(response.status());
                last_error = upstream_error(response).await;
                if !should_next {
                    return Err(last_error);
                }
            }
            Err(error) => last_error = DispatchError::bad_gateway(error),
        }
    }
    Err(last_error)
}

async fn ensure_fresh_and_save(state: &AppState, account: &mut Account) -> Result<()> {
    if ensure_fresh(account, &state.client).await? {
        state.store.save_account(account)?;
    }
    Ok(())
}

async fn force_refresh_and_save(state: &AppState, account: &mut Account) -> Result<()> {
    let refresh = account
        .refresh_token
        .clone()
        .ok_or_else(|| anyhow!("missing refresh token"))?;
    let tokens =
        crate::account::refresh_access_token(&state.client, &refresh, account.id_token.as_deref())
            .await?;
    account.id_token = tokens.id_token.or_else(|| account.id_token.clone());
    account.access_token = tokens.access_token;
    account.refresh_token = tokens.refresh_token.or(Some(refresh));
    account.updated_at = Utc::now().timestamp_millis();
    state.store.save_account(account)?;
    Ok(())
}

async fn send_upstream(
    state: &AppState,
    request: &ParsedGatewayRequest,
    account: &Account,
) -> Result<reqwest::Response> {
    let upstream_target = resolve_upstream_target(&request.target)?;
    let url = format!("{}{}", state.upstream_base_url, upstream_target);
    let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes())
        .context("convert upstream method")?;
    let mut builder = state.client.request(method, url);
    for (name, value) in request.headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization"
                | "host"
                | "content-length"
                | "connection"
                | "accept-encoding"
                | "x-api-key"
        ) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder = builder.header(
        AUTHORIZATION,
        format!("Bearer {}", account.access_token.trim()),
    );
    builder = builder.header(USER_AGENT, DEFAULT_CODEX_USER_AGENT);
    builder = builder.header("Originator", DEFAULT_CODEX_ORIGINATOR);
    if let Some(account_id) = account.account_id.as_deref() {
        builder = builder.header("ChatGPT-Account-Id", account_id);
    }
    if !request.headers.contains_key(ACCEPT) {
        builder = builder.header(ACCEPT, "application/json");
    }
    if !request.headers.contains_key(CONTENT_TYPE) && !request.body.is_empty() {
        builder = builder.header(CONTENT_TYPE, "application/json");
    }
    if !request.body.is_empty() {
        builder = builder.body(request.body.clone());
    }
    builder.send().await.context("send upstream request")
}

async fn adapt_response(
    response: reqwest::Response,
    adapter: ResponseAdapter,
    state: &AppState,
    account_id: String,
) -> Response {
    let status = response.status();
    let headers = response.headers().clone();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                format!("read upstream failed: {}", error),
            )
        }
    };
    if !status.is_success() {
        return bytes_response(status, &headers, body);
    }
    match adapter {
        ResponseAdapter::Passthrough => {
            if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                if let Some(id) = extract_response_id(&value) {
                    state.affinity.lock().await.insert(id, account_id);
                }
            }
            bytes_response(status, &headers, body)
        }
        ResponseAdapter::ChatCompletions => match serde_json::from_slice::<Value>(&body) {
            Ok(value) => {
                if let Some(id) = extract_response_id(&value) {
                    state.affinity.lock().await.insert(id, account_id);
                }
                json_ok(build_chat_completion_payload(&value))
            }
            Err(error) => json_error(
                StatusCode::BAD_GATEWAY,
                format!("parse upstream chat payload failed: {}", error),
            ),
        },
        ResponseAdapter::Images { response_format } => match serde_json::from_slice::<Value>(&body)
        {
            Ok(value) => json_ok(build_images_api_payload(&value, &response_format)),
            Err(error) => json_error(
                StatusCode::BAD_GATEWAY,
                format!("parse upstream image payload failed: {}", error),
            ),
        },
    }
}

fn prepare_gateway_request(
    mut request: ParsedGatewayRequest,
) -> Result<(ParsedGatewayRequest, ResponseAdapter)> {
    if path_is(&request.target, CHAT_COMPLETIONS_PATH) {
        if request.method != Method::POST {
            return Err(anyhow!("chat/completions only supports POST"));
        }
        let body: Value =
            serde_json::from_slice(&request.body).context("chat/completions body must be JSON")?;
        request.body = Bytes::from(serde_json::to_vec(&build_responses_body_from_chat(&body))?);
        request.target = RESPONSES_PATH.to_string();
        return Ok((request, ResponseAdapter::ChatCompletions));
    }
    if path_is(&request.target, IMAGES_GENERATIONS_PATH) {
        if request.method != Method::POST {
            return Err(anyhow!("images/generations only supports POST"));
        }
        let body: Value = serde_json::from_slice(&request.body)
            .context("images/generations body must be JSON")?;
        let response_format = image_response_format(&body);
        request.body = Bytes::from(serde_json::to_vec(&build_images_generation_request(
            &body,
        )?)?);
        request.target = RESPONSES_PATH.to_string();
        return Ok((request, ResponseAdapter::Images { response_format }));
    }
    if path_is(&request.target, IMAGES_EDITS_PATH) {
        if request.method != Method::POST {
            return Err(anyhow!("images/edits only supports POST"));
        }
        let (body, response_format) = build_images_edit_request(&request.headers, &request.body)?;
        request.body = Bytes::from(serde_json::to_vec(&body)?);
        request.target = RESPONSES_PATH.to_string();
        return Ok((request, ResponseAdapter::Images { response_format }));
    }
    if path_is(&request.target, RESPONSES_PATH) {
        if !request.body.is_empty() {
            if let Ok(mut body) = serde_json::from_slice::<Value>(&request.body) {
                rewrite_model_alias(&mut body);
                inject_image_tool(&mut body);
                request.body = Bytes::from(serde_json::to_vec(&body)?);
            }
        }
        return Ok((request, ResponseAdapter::Passthrough));
    }
    Ok((request, ResponseAdapter::Passthrough))
}

fn build_responses_body_from_chat(body: &Value) -> Value {
    let model = body
        .get("model")
        .cloned()
        .unwrap_or_else(|| json!("gpt-5-codex"));
    let messages = body.get("messages").cloned().unwrap_or_else(|| json!([]));
    let mut input = Vec::new();
    if let Some(messages) = messages.as_array() {
        for message in messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let content = message
                .get("content")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            input.push(json!({
                "role": role,
                "content": normalize_chat_content(content),
            }));
        }
    }
    let mut out = json!({
        "model": model,
        "input": input,
        "stream": body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        "store": body.get("store").and_then(Value::as_bool).unwrap_or(false),
    });
    if let Some(tools) = body.get("tools") {
        out["tools"] = tools.clone();
    }
    inject_image_tool(&mut out);
    out
}

fn normalize_chat_content(content: Value) -> Value {
    match content {
        Value::String(text) => json!([{ "type": "input_text", "text": text }]),
        Value::Array(parts) => Value::Array(
            parts
                .into_iter()
                .filter_map(|part| {
                    let kind = part.get("type").and_then(Value::as_str)?;
                    match kind {
                        "text" | "input_text" => Some(json!({
                            "type": "input_text",
                            "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
                        })),
                        "image_url" => Some(json!({
                            "type": "input_image",
                            "image_url": part
                                .get("image_url")
                                .and_then(|value| value.get("url"))
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                        })),
                        _ => Some(part),
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

fn build_images_generation_request(body: &Value) -> Result<Value> {
    validate_image_model(body)?;
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("images/generations missing prompt"))?;
    let tool = image_generation_tool(body);
    Ok(json!({
        "model": DEFAULT_IMAGES_MAIN_MODEL,
        "input": prompt,
        "tools": [tool],
        "tool_choice": {"type": "image_generation"},
        "store": false,
        "stream": false
    }))
}

fn build_images_edit_request(headers: &HeaderMap, body: &[u8]) -> Result<(Value, String)> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        let form = parse_multipart_form(content_type, body)?;
        let prompt = form
            .fields
            .get("prompt")
            .cloned()
            .ok_or_else(|| anyhow!("images/edits multipart missing prompt"))?;
        if form.images.is_empty() {
            return Err(anyhow!("images/edits multipart missing image"));
        }
        let raw = form.raw_fields;
        let tool = image_generation_tool(&raw);
        let response_format = image_response_format(&raw);
        return Ok((
            build_image_responses_body(&prompt, &form.images, tool),
            response_format,
        ));
    }
    let body: Value =
        serde_json::from_slice(body).context("images/edits body must be JSON or multipart")?;
    validate_image_model(&body)?;
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("images/edits missing prompt"))?;
    let images = extract_json_edit_images(&body);
    if images.is_empty() {
        return Err(anyhow!("images/edits missing image or images[].image_url"));
    }
    let response_format = image_response_format(&body);
    let tool = image_generation_tool(&body);
    Ok((
        build_image_responses_body(prompt, &images, tool),
        response_format,
    ))
}

fn build_image_responses_body(prompt: &str, images: &[String], tool: Value) -> Value {
    let mut content = vec![json!({"type": "input_text", "text": prompt})];
    for image in images {
        content.push(json!({"type": "input_image", "image_url": image}));
    }
    json!({
        "model": DEFAULT_IMAGES_MAIN_MODEL,
        "input": [{"role": "user", "content": content}],
        "tools": [tool],
        "tool_choice": {"type": "image_generation"},
        "store": false,
        "stream": false
    })
}

fn image_generation_tool(body: &Value) -> Value {
    let mut tool = Map::new();
    tool.insert("type".to_string(), json!("image_generation"));
    for key in [
        "size",
        "quality",
        "background",
        "output_format",
        "output_compression",
        "partial_images",
    ] {
        if let Some(value) = body.get(key).filter(|value| !value.is_null()) {
            tool.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(tool)
}

fn validate_image_model(body: &Value) -> Result<()> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(CODEX_IMAGE_MODEL_ID);
    if model == CODEX_IMAGE_MODEL_ID {
        Ok(())
    } else {
        Err(anyhow!(
            "unsupported image model {}; expected {}",
            model,
            CODEX_IMAGE_MODEL_ID
        ))
    }
}

fn inject_image_tool(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let tools = object
        .entry("tools")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(tools) = tools.as_array_mut() else {
        return;
    };
    if !tools
        .iter()
        .any(|tool| tool.get("type").and_then(Value::as_str) == Some("image_generation"))
    {
        tools.push(json!({"type":"image_generation"}));
    }
}

fn rewrite_model_alias(body: &mut Value) {
    let Some(model) = body.get_mut("model") else {
        return;
    };
    if model.as_str() == Some("codex-mini-latest") {
        *model = json!("gpt-5-codex-mini");
    }
}

fn build_models_response() -> Value {
    let models = [
        "gpt-5-codex",
        "gpt-5-codex-mini",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.3-codex",
        "gpt-5.3-codex-spark",
        "gpt-5.2",
        "gpt-5.2-codex",
        "gpt-5.1-codex-max",
        "gpt-5.1-codex-mini",
        CODEX_IMAGE_MODEL_ID,
    ];
    json!({
        "object": "list",
        "data": models.into_iter().map(|id| json!({
            "id": id,
            "object": "model",
            "created": 0,
            "owned_by": "openai"
        })).collect::<Vec<_>>()
    })
}

fn build_chat_completion_payload(response: &Value) -> Value {
    let id = extract_response_id(response)
        .unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::new_v4()));
    json!({
        "id": id,
        "object": "chat.completion",
        "created": Utc::now().timestamp(),
        "model": response.get("model").cloned().unwrap_or_else(|| json!("gpt-5-codex")),
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": extract_output_text(response)
            },
            "finish_reason": "stop"
        }],
        "usage": response.get("usage").cloned().unwrap_or(Value::Null)
    })
}

fn build_images_api_payload(response: &Value, response_format: &str) -> Value {
    let mut data = Vec::new();
    for image in extract_images(response) {
        let item = if response_format == "url" && image.starts_with("http") {
            json!({"url": image})
        } else {
            json!({"b64_json": image})
        };
        data.push(item);
    }
    json!({
        "created": Utc::now().timestamp(),
        "data": data
    })
}

fn extract_images(value: &Value) -> Vec<String> {
    let mut images = Vec::new();
    collect_image_values(value, &mut images);
    images
}

fn collect_image_values(value: &Value, images: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for key in ["result", "b64_json", "image_url", "url"] {
                if let Some(text) = object.get(key).and_then(Value::as_str) {
                    if text.starts_with("data:image/") {
                        if let Some((_, data)) = text.split_once(',') {
                            images.push(data.to_string());
                        }
                    } else if text.len() > 64 || text.starts_with("http") {
                        images.push(text.to_string());
                    }
                }
            }
            for value in object.values() {
                collect_image_values(value, images);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_image_values(value, images);
            }
        }
        _ => {}
    }
}

fn extract_output_text(response: &Value) -> String {
    if let Some(text) = response.get("output_text").and_then(Value::as_str) {
        return text.to_string();
    }
    let mut out = String::new();
    collect_text(response.get("output").unwrap_or(response), &mut out);
    out
}

fn extract_output_text_from_response_text(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .map(|value| extract_output_text(&value))
        .unwrap_or_else(|_| text.to_string())
}

fn collect_text(value: &Value, out: &mut String) {
    match value {
        Value::Object(object) => {
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("output_text" | "text")
            ) {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
            for value in object.values() {
                collect_text(value, out);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_text(value, out);
            }
        }
        _ => {}
    }
}

fn extract_response_id(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("response")
                .and_then(|v| v.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}

fn previous_response_id(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("previous_response_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn extract_json_edit_images(body: &Value) -> Vec<String> {
    let mut images = Vec::new();
    if let Some(image) = body.get("image").and_then(Value::as_str) {
        images.push(image.to_string());
    }
    if let Some(items) = body.get("images").and_then(Value::as_array) {
        for item in items {
            if let Some(url) = item
                .get("image_url")
                .and_then(|value| value.get("url").or(Some(value)))
                .and_then(Value::as_str)
            {
                images.push(url.to_string());
            }
        }
    }
    images
}

#[derive(Default)]
struct ParsedMultipart {
    fields: HashMap<String, String>,
    raw_fields: Value,
    images: Vec<String>,
}

fn parse_multipart_form(content_type: &str, body: &[u8]) -> Result<ParsedMultipart> {
    let boundary = content_type
        .split(';')
        .find_map(|part| part.trim().strip_prefix("boundary="))
        .map(|value| value.trim_matches('"'))
        .ok_or_else(|| anyhow!("multipart missing boundary"))?;
    let marker = format!("--{}", boundary).into_bytes();
    let mut parsed = ParsedMultipart::default();
    let mut raw = Map::new();
    for part in split_multipart_parts(body, &marker) {
        if part.is_empty() || part.starts_with(b"--") {
            continue;
        }
        let Some(header_end) = find_subslice(part, b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&part[..header_end]);
        let mut value = &part[header_end + 4..];
        value = trim_part_suffix(value);
        let Some(name) = multipart_name(&headers) else {
            continue;
        };
        if name == "image" || name == "images" {
            let data = general_purpose::STANDARD.encode(value);
            let mime = multipart_content_type(&headers).unwrap_or_else(|| detect_image_mime(value));
            parsed.images.push(format!("data:{};base64,{}", mime, data));
        } else {
            let value = String::from_utf8_lossy(value).trim().to_string();
            parsed.fields.insert(name.to_string(), value.clone());
            raw.insert(name.to_string(), Value::String(value));
        }
    }
    parsed.raw_fields = Value::Object(raw);
    Ok(parsed)
}

fn split_multipart_parts<'a>(body: &'a [u8], marker: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut cursor = 0usize;
    while let Some(start) = find_subslice(&body[cursor..], marker) {
        let part_start = cursor + start + marker.len();
        let next_search = part_start;
        let next = find_subslice(&body[next_search..], marker)
            .map(|offset| next_search + offset)
            .unwrap_or(body.len());
        parts.push(trim_part_prefix(&body[part_start..next]));
        cursor = next;
        if next >= body.len() {
            break;
        }
    }
    parts
}

fn trim_part_prefix(mut value: &[u8]) -> &[u8] {
    if value.starts_with(b"\r\n") {
        value = &value[2..];
    }
    value
}

fn trim_part_suffix(mut value: &[u8]) -> &[u8] {
    if value.ends_with(b"\r\n") {
        value = &value[..value.len() - 2];
    }
    if value.ends_with(b"--") {
        value = &value[..value.len() - 2];
    }
    value
}

fn multipart_name(headers: &str) -> Option<&str> {
    headers
        .split(';')
        .find_map(|item| item.trim().strip_prefix("name=\""))
        .and_then(|item| item.split('"').next())
}

fn multipart_content_type(headers: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-type")
            .then(|| value.trim().to_string())
    })
}

fn detect_image_mime(data: &[u8]) -> String {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png".to_string()
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg".to_string()
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "image/gif".to_string()
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        "image/webp".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn image_response_format(body: &Value) -> String {
    body.get("response_format")
        .or_else(|| body.get("output_format"))
        .and_then(Value::as_str)
        .unwrap_or("b64_json")
        .to_string()
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(str::trim);
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    bearer == Some(expected) || api_key == Some(expected)
}

fn normalize_target(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string())
}

fn resolve_upstream_target(target: &str) -> Result<String> {
    if !target.starts_with("/v1") {
        return Err(anyhow!("only /v1 paths are supported"));
    }
    let trimmed = target.trim_start_matches("/v1");
    if trimmed.is_empty() {
        Ok("/".to_string())
    } else if trimmed.starts_with('/') {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("/{}", trimmed))
    }
}

fn path_is(target: &str, expected: &str) -> bool {
    target == expected || target.starts_with(&format!("{}?", expected))
}

fn is_models_request(target: &str) -> bool {
    path_is(target, "/v1/models")
}

fn should_try_next_account(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::INTERNAL_SERVER_ERROR
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    )
}

async fn upstream_error(response: reqwest::Response) -> DispatchError {
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|v| v.get("message").or(Some(v)))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
        })
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                format!("upstream returned {}", status)
            } else {
                body
            }
        });
    DispatchError::new(status, message)
}

struct DispatchError {
    status: StatusCode,
    message: String,
}

impl DispatchError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_gateway(error: anyhow::Error) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, error.to_string())
    }

    fn unavailable(error: anyhow::Error) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
    }
}

fn json_ok(value: Value) -> Response {
    json_response(StatusCode::OK, value)
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    json_response(
        status,
        json!({
            "error": {
                "message": message.into(),
                "type": "codex_gateway_error",
                "code": status.as_u16()
            }
        }),
    )
}

fn json_response(status: StatusCode, value: Value) -> Response {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    response
}

fn bytes_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: Bytes,
) -> Response {
    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, body).into_response();
    if let Some(content_type) = headers.get(CONTENT_TYPE) {
        if let Ok(content_type) = HeaderValue::from_bytes(content_type.as_bytes()) {
            response.headers_mut().insert(CONTENT_TYPE, content_type);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_include_image_model() {
        let response = build_models_response();
        let data = response.get("data").and_then(Value::as_array).unwrap();
        assert!(data
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some("gpt-image-2")));
    }

    #[test]
    fn chat_completions_are_prepared_as_responses() {
        let request = ParsedGatewayRequest {
            method: Method::POST,
            target: "/v1/chat/completions".to_string(),
            headers: HeaderMap::new(),
            body: Bytes::from_static(
                br#"{"model":"gpt-5-codex","messages":[{"role":"user","content":"hi"}]}"#,
            ),
        };
        let (prepared, adapter) = prepare_gateway_request(request).unwrap();
        assert_eq!(prepared.target, "/v1/responses");
        assert!(matches!(adapter, ResponseAdapter::ChatCompletions));
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn images_generation_is_prepared_as_responses() {
        let request = ParsedGatewayRequest {
            method: Method::POST,
            target: "/v1/images/generations".to_string(),
            headers: HeaderMap::new(),
            body: Bytes::from_static(
                br#"{"model":"gpt-image-2","prompt":"draw","size":"1024x1024"}"#,
            ),
        };
        let (prepared, adapter) = prepare_gateway_request(request).unwrap();
        assert_eq!(prepared.target, "/v1/responses");
        assert!(matches!(adapter, ResponseAdapter::Images { .. }));
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["tools"][0]["type"], "image_generation");
    }

    #[test]
    fn multipart_images_edit_preserves_binary_image() {
        let boundary = "test-boundary";
        let png = b"\x89PNG\r\n\x1a\nabc";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit it\r\n",
                boundary
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n",
                boundary
            )
            .as_bytes(),
        );
        body.extend_from_slice(png);
        body.extend_from_slice(format!("\r\n--{}--\r\n", boundary).as_bytes());

        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={}", boundary)).unwrap(),
        );
        let (prepared, response_format) = build_images_edit_request(&headers, &body).unwrap();
        assert_eq!(response_format, "b64_json");
        let image_url = prepared["input"][0]["content"][1]["image_url"]
            .as_str()
            .unwrap();
        assert!(image_url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn responses_rewrite_model_alias_and_inject_image_tool() {
        let request = ParsedGatewayRequest {
            method: Method::POST,
            target: "/v1/responses".to_string(),
            headers: HeaderMap::new(),
            body: Bytes::from_static(br#"{"model":"codex-mini-latest","input":"hi"}"#),
        };
        let (prepared, adapter) = prepare_gateway_request(request).unwrap();
        assert!(matches!(adapter, ResponseAdapter::Passthrough));
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["model"], "gpt-5-codex-mini");
        assert_eq!(body["tools"][0]["type"], "image_generation");
    }

    #[test]
    fn rejects_wrong_image_model() {
        let request = ParsedGatewayRequest {
            method: Method::POST,
            target: "/v1/images/generations".to_string(),
            headers: HeaderMap::new(),
            body: Bytes::from_static(br#"{"model":"other","prompt":"draw"}"#),
        };
        assert!(prepare_gateway_request(request).is_err());
    }

    #[test]
    fn bearer_and_x_api_key_authenticate() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        assert!(authorized(&headers, "secret"));
        headers.clear();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        assert!(authorized(&headers, "secret"));
        assert!(!authorized(&headers, "nope"));
    }
}
