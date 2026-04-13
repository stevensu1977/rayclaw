// ---------------------------------------------------------------------------
// Amazon Bedrock Converse API provider (native, NOT invoke_model)
//
// Endpoints:
//   POST /model/{modelId}/converse
//   POST /model/{modelId}/converse-stream
//
// Authentication: AWS SigV4 signing
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use crate::config::Config;
use crate::error::RayClawError;
use crate::llm::{normalize_stop_reason, sanitize_messages, LlmProvider};
use crate::llm_types::{
    ContentBlock, Message, MessageContent, MessagesResponse, ResponseContentBlock, ToolDefinition,
    Usage,
};

// ---------------------------------------------------------------------------
// AWS Credentials
// ---------------------------------------------------------------------------

pub(crate) struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: String,
}

impl AwsCredentials {
    pub fn resolve(config: &Config) -> Result<Self, RayClawError> {
        let access_key = config
            .aws_access_key_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok())
            .filter(|s| !s.trim().is_empty());

        let secret_key = config
            .aws_secret_access_key
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok())
            .filter(|s| !s.trim().is_empty());

        let session_token = config
            .aws_session_token
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("AWS_SESSION_TOKEN").ok())
            .filter(|s| !s.trim().is_empty());

        let region = config
            .aws_region
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("AWS_REGION").ok())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
            .filter(|s| !s.trim().is_empty());

        // If no explicit credentials, try ~/.aws/credentials
        let (access_key, secret_key, session_token, region) =
            if let (Some(ak), Some(sk)) = (access_key.clone(), secret_key.clone()) {
                (ak, sk, session_token, region)
            } else {
                let profile_name = config
                    .aws_profile
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| std::env::var("AWS_PROFILE").ok())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "default".into());

                let (file_ak, file_sk, file_token) = parse_aws_credentials_file(&profile_name);
                let ak = access_key.or(file_ak);
                let sk = secret_key.or(file_sk);
                let token = session_token.or(file_token);

                // Also check ~/.aws/config for region
                let region = region.or_else(|| parse_aws_config_region(&profile_name));

                (
                    ak.unwrap_or_default(),
                    sk.unwrap_or_default(),
                    token,
                    region,
                )
            };

        // Last resort: EC2 Instance Metadata Service (IMDSv2)
        let (access_key, secret_key, session_token, region) = if !access_key.is_empty()
            && !secret_key.is_empty()
        {
            (access_key, secret_key, session_token, region)
        } else {
            match fetch_imds_credentials() {
                Ok((ak, sk, token, imds_region)) => {
                    let region = region.or(imds_region);
                    (ak, sk, Some(token), region)
                }
                Err(_) => {
                    return Err(RayClawError::Config(
                            "AWS credentials not found. Set aws_access_key_id/aws_secret_access_key in config, \
                             AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY env vars, \
                             configure ~/.aws/credentials, \
                             or attach an IAM role to your EC2 instance"
                                .into(),
                        ));
                }
            }
        };

        let region = region.unwrap_or_else(|| "us-east-1".into());

        Ok(AwsCredentials {
            access_key_id: access_key,
            secret_access_key: secret_key,
            session_token,
            region,
        })
    }
}

fn parse_aws_credentials_file(profile: &str) -> (Option<String>, Option<String>, Option<String>) {
    let path = dirs_or_home().join(".aws").join("credentials");
    parse_ini_profile(
        &path,
        profile,
        &[
            "aws_access_key_id",
            "aws_secret_access_key",
            "aws_session_token",
        ],
    )
    .map(|vals| {
        (
            vals.get("aws_access_key_id").cloned(),
            vals.get("aws_secret_access_key").cloned(),
            vals.get("aws_session_token").cloned(),
        )
    })
    .unwrap_or((None, None, None))
}

fn parse_aws_config_region(profile: &str) -> Option<String> {
    let path = dirs_or_home().join(".aws").join("config");
    // In ~/.aws/config, profiles are [profile name] except [default]
    let section = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    parse_ini_profile(&path, &section, &["region"]).and_then(|vals| vals.get("region").cloned())
}

/// Fetch temporary credentials from EC2 Instance Metadata Service (IMDSv2).
/// Returns (access_key, secret_key, session_token, optional_region).
fn fetch_imds_credentials() -> Result<(String, String, String, Option<String>), RayClawError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|e| RayClawError::Config(format!("IMDS HTTP client error: {e}")))?;

    // IMDSv2: get a session token first
    let token = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
        .send()
        .and_then(|r| r.text())
        .map_err(|e| RayClawError::Config(format!("IMDS token request failed: {e}")))?;

    // Get the IAM role name
    let role = client
        .get("http://169.254.169.254/latest/meta-data/iam/security-credentials/")
        .header("X-aws-ec2-metadata-token", &token)
        .send()
        .and_then(|r| r.text())
        .map_err(|e| RayClawError::Config(format!("IMDS role lookup failed: {e}")))?;
    let role = role.trim().to_string();
    if role.is_empty() {
        return Err(RayClawError::Config(
            "No IAM role attached to this EC2 instance".into(),
        ));
    }

    // Get credentials for the role
    let creds_url =
        format!("http://169.254.169.254/latest/meta-data/iam/security-credentials/{role}");
    let creds_json: serde_json::Value = client
        .get(&creds_url)
        .header("X-aws-ec2-metadata-token", &token)
        .send()
        .and_then(|r| r.json())
        .map_err(|e| RayClawError::Config(format!("IMDS credentials fetch failed: {e}")))?;

    let ak = creds_json["AccessKeyId"].as_str().unwrap_or("").to_string();
    let sk = creds_json["SecretAccessKey"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let session_token = creds_json["Token"].as_str().unwrap_or("").to_string();

    if ak.is_empty() || sk.is_empty() {
        return Err(RayClawError::Config(
            "IMDS returned empty credentials".into(),
        ));
    }

    // Try to get region from IMDS placement data
    let region = client
        .get("http://169.254.169.254/latest/meta-data/placement/region")
        .header("X-aws-ec2-metadata-token", &token)
        .send()
        .and_then(|r| r.text())
        .ok()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());

    Ok((ak, sk, session_token, region))
}

fn dirs_or_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/root"))
}

fn parse_ini_profile(
    path: &std::path::Path,
    profile: &str,
    keys: &[&str],
) -> Option<std::collections::HashMap<String, String>> {
    let content = std::fs::read_to_string(path).ok()?;
    let target_header = format!("[{profile}]");
    let mut in_section = false;
    let mut result = std::collections::HashMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == target_header;
            continue;
        }
        if in_section {
            if let Some((key, value)) = trimmed.split_once('=') {
                let k = key.trim();
                let v = value.trim();
                if keys.contains(&k) {
                    result.insert(k.to_string(), v.to_string());
                }
            }
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

// ---------------------------------------------------------------------------
// AWS SigV4 Signing
// ---------------------------------------------------------------------------

type HmacSha256 = Hmac<Sha256>;

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key size");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn sigv4_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Sign a request and return the headers to add (Authorization, X-Amz-Date, optionally X-Amz-Security-Token).
#[allow(clippy::too_many_arguments)]
fn sign_request(
    method: &str,
    url: &reqwest::Url,
    body: &[u8],
    region: &str,
    service: &str,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    now: &chrono::DateTime<chrono::Utc>,
) -> Vec<(String, String)> {
    let date_stamp = now.format("%Y%m%d").to_string();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();

    let host = url.host_str().unwrap_or("");
    let path = url.path();

    let payload_hash = sha256_hex(body);

    // Canonical headers (must be sorted)
    let mut signed_headers_list = vec!["host", "x-amz-date"];
    let mut canonical_headers = format!("host:{host}\nx-amz-date:{amz_date}\n");

    if let Some(token) = session_token {
        signed_headers_list.push("x-amz-security-token");
        canonical_headers.push_str(&format!("x-amz-security-token:{token}\n"));
    }
    // Ensure sorted order
    signed_headers_list.sort();
    let signed_headers = signed_headers_list.join(";");

    // Rebuild canonical headers in sorted order
    let mut sorted_headers = String::new();
    for h in &signed_headers_list {
        match *h {
            "host" => sorted_headers.push_str(&format!("host:{host}\n")),
            "x-amz-date" => sorted_headers.push_str(&format!("x-amz-date:{amz_date}\n")),
            "x-amz-security-token" => {
                sorted_headers.push_str(&format!(
                    "x-amz-security-token:{}\n",
                    session_token.unwrap_or("")
                ));
            }
            _ => {}
        }
    }

    let canonical_request =
        format!("{method}\n{path}\n\n{sorted_headers}\n{signed_headers}\n{payload_hash}");

    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let signing_key = sigv4_signing_key(secret_key, &date_stamp, region, service);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );

    let mut headers = vec![
        ("Authorization".into(), authorization),
        ("X-Amz-Date".into(), amz_date),
    ];
    if let Some(token) = session_token {
        headers.push(("X-Amz-Security-Token".into(), token.to_string()));
    }
    headers
}

// ---------------------------------------------------------------------------
// AWS Event Stream binary parser
// ---------------------------------------------------------------------------

struct EventStreamParser {
    buffer: Vec<u8>,
}

impl EventStreamParser {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    fn feed(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Try to extract the next complete frame. Returns the JSON payload and event type.
    fn next_frame(&mut self) -> Option<(String, serde_json::Value)> {
        // Minimum frame size: 4 (total_len) + 4 (headers_len) + 4 (prelude_crc) + 4 (msg_crc) = 16
        if self.buffer.len() < 12 {
            return None;
        }

        let total_len = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;

        if total_len < 16 || self.buffer.len() < total_len {
            return None;
        }

        let headers_len = u32::from_be_bytes([
            self.buffer[4],
            self.buffer[5],
            self.buffer[6],
            self.buffer[7],
        ]) as usize;

        // Validate prelude CRC
        let prelude_crc_expected = u32::from_be_bytes([
            self.buffer[8],
            self.buffer[9],
            self.buffer[10],
            self.buffer[11],
        ]);
        let prelude_crc_actual = crc32fast::hash(&self.buffer[..8]);
        if prelude_crc_expected != prelude_crc_actual {
            warn!("Event Stream prelude CRC mismatch, skipping frame");
            self.buffer.drain(..total_len);
            return None;
        }

        // Parse headers to find :event-type
        let headers_start = 12;
        let headers_end = headers_start + headers_len;
        let event_type = parse_event_type(&self.buffer[headers_start..headers_end]);

        // Payload is between headers_end and (total_len - 4) for msg CRC
        let payload_end = total_len - 4;
        let payload = if payload_end > headers_end {
            &self.buffer[headers_end..payload_end]
        } else {
            &[]
        };

        let json_value = if payload.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(payload).unwrap_or(serde_json::Value::Null)
        };

        self.buffer.drain(..total_len);
        Some((event_type, json_value))
    }
}

/// Parse headers section to extract `:event-type` value.
fn parse_event_type(mut data: &[u8]) -> String {
    while data.len() > 2 {
        let name_len = data[0] as usize;
        if data.len() < 1 + name_len + 1 {
            break;
        }
        let name = &data[1..1 + name_len];
        let rest = &data[1 + name_len..];

        // Header type byte
        if rest.is_empty() {
            break;
        }
        let header_type = rest[0];
        let rest = &rest[1..];

        match header_type {
            // Type 7 = String
            7 => {
                if rest.len() < 2 {
                    break;
                }
                let val_len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                if rest.len() < 2 + val_len {
                    break;
                }
                let val = &rest[2..2 + val_len];
                if name == b":event-type" || name == b":exception-type" {
                    return String::from_utf8_lossy(val).to_string();
                }
                data = &rest[2 + val_len..];
            }
            // Type 0 = Bool true (0 bytes)
            0 => {
                data = rest;
            }
            // Type 1 = Bool false (0 bytes)
            1 => {
                data = rest;
            }
            // Type 2 = Byte (1 byte)
            2 => {
                if rest.is_empty() {
                    break;
                }
                data = &rest[1..];
            }
            // Type 3 = Short (2 bytes)
            3 => {
                if rest.len() < 2 {
                    break;
                }
                data = &rest[2..];
            }
            // Type 4 = Int (4 bytes)
            4 => {
                if rest.len() < 4 {
                    break;
                }
                data = &rest[4..];
            }
            // Type 5 = Long (8 bytes)
            5 => {
                if rest.len() < 8 {
                    break;
                }
                data = &rest[8..];
            }
            // Type 6 = Bytes (2-byte len prefix)
            6 => {
                if rest.len() < 2 {
                    break;
                }
                let val_len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                if rest.len() < 2 + val_len {
                    break;
                }
                data = &rest[2 + val_len..];
            }
            // Type 8 = Timestamp (8 bytes)
            8 => {
                if rest.len() < 8 {
                    break;
                }
                data = &rest[8..];
            }
            // Type 9 = UUID (16 bytes)
            9 => {
                if rest.len() < 16 {
                    break;
                }
                data = &rest[16..];
            }
            _ => break,
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Message translation: internal types ↔ Bedrock Converse format
// ---------------------------------------------------------------------------

fn translate_messages_to_bedrock(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter_map(|msg| {
            let content = match &msg.content {
                MessageContent::Text(text) => {
                    if text.trim().is_empty() {
                        return None;
                    }
                    vec![serde_json::json!({ "text": text })]
                }
                MessageContent::Blocks(blocks) => {
                    let filtered: Vec<_> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } if text.trim().is_empty() => None,
                            ContentBlock::Text { text } => {
                                Some(serde_json::json!({ "text": text }))
                            }
                            ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
                                "toolUse": {
                                    "toolUseId": id,
                                    "name": name,
                                    "input": input,
                                }
                            })),
                            ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } => {
                                let status = if is_error.unwrap_or(false) {
                                    "error"
                                } else {
                                    "success"
                                };
                                let content_blocks = if let Ok(json_val) =
                                    serde_json::from_str::<serde_json::Value>(content)
                                {
                                    vec![serde_json::json!({ "json": json_val })]
                                } else {
                                    vec![serde_json::json!({ "text": content })]
                                };
                                Some(serde_json::json!({
                                    "toolResult": {
                                        "toolUseId": tool_use_id,
                                        "content": content_blocks,
                                        "status": status,
                                    }
                                }))
                            }
                            ContentBlock::Image { source } => {
                                let format = mime_to_bedrock_format(&source.media_type);
                                Some(serde_json::json!({
                                    "image": {
                                        "format": format,
                                        "source": {
                                            "bytes": source.data,
                                        }
                                    }
                                }))
                            }
                        })
                        .collect();
                    if filtered.is_empty() {
                        return None;
                    }
                    filtered
                }
            };
            Some(serde_json::json!({
                "role": msg.role,
                "content": content,
            }))
        })
        .collect()
}

fn translate_tools_to_bedrock(tools: &[ToolDefinition]) -> serde_json::Value {
    let tool_specs: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "toolSpec": {
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": {
                        "json": t.input_schema,
                    }
                }
            })
        })
        .collect();
    serde_json::json!({ "tools": tool_specs })
}

fn translate_bedrock_response(body: &serde_json::Value) -> MessagesResponse {
    let mut content = Vec::new();

    if let Some(output) = body.get("output") {
        if let Some(message) = output.get("message") {
            if let Some(blocks) = message.get("content").and_then(|c| c.as_array()) {
                for block in blocks {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        content.push(ResponseContentBlock::Text {
                            text: text.to_string(),
                        });
                    } else if let Some(tool_use) = block.get("toolUse") {
                        let id = tool_use
                            .get("toolUseId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = tool_use
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = tool_use
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        content.push(ResponseContentBlock::ToolUse { id, name, input });
                    }
                }
            }
        }
    }

    let stop_reason = body
        .get("stopReason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let usage = body.get("usage").map(|u| Usage {
        input_tokens: u.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        output_tokens: u.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
    });

    MessagesResponse {
        content,
        stop_reason: normalize_stop_reason(stop_reason),
        usage,
    }
}

fn mime_to_bedrock_format(mime: &str) -> &str {
    match mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpeg",
    }
}

// ---------------------------------------------------------------------------
// BedrockProvider
// ---------------------------------------------------------------------------

pub struct BedrockProvider {
    http: reqwest::Client,
    credentials: AwsCredentials,
    model: String,
    max_tokens: u32,
    prompt_cache_ttl: String,
    idle_timeout: std::time::Duration,
}

impl BedrockProvider {
    pub fn new(config: &Config) -> Result<Self, RayClawError> {
        let credentials = AwsCredentials::resolve(config)?;
        Ok(BedrockProvider {
            http: reqwest::Client::new(),
            credentials,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            prompt_cache_ttl: config.prompt_cache_ttl.clone(),
            idle_timeout: std::time::Duration::from_secs(config.llm_idle_timeout_secs.max(5)),
        })
    }

    fn converse_url(&self) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse",
            self.credentials.region,
            urlencoding::encode(&self.model)
        )
    }

    fn converse_stream_url(&self) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse-stream",
            self.credentials.region,
            urlencoding::encode(&self.model)
        )
    }

    fn build_request_body(
        &self,
        system: &str,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> serde_json::Value {
        let use_cache = self.prompt_cache_ttl != "none";

        let mut body = serde_json::json!({
            "messages": translate_messages_to_bedrock(messages),
            "inferenceConfig": {
                "maxTokens": self.max_tokens,
            },
        });

        if !system.is_empty() {
            if use_cache {
                // Add system prompt with cache point
                body["system"] = serde_json::json!([
                    { "text": system },
                    { "cachePoint": { "type": "default", "ttl": self.prompt_cache_ttl } }
                ]);
            } else {
                body["system"] = serde_json::json!([{ "text": system }]);
            }
        }

        if let Some(tools) = tools {
            if !tools.is_empty() {
                let mut tool_config = translate_tools_to_bedrock(tools);

                if use_cache {
                    // Add cache point after the last tool
                    if let Some(tools_array) =
                        tool_config.get_mut("tools").and_then(|t| t.as_array_mut())
                    {
                        tools_array.push(serde_json::json!({
                            "cachePoint": { "type": "default", "ttl": self.prompt_cache_ttl }
                        }));
                    }
                }

                body["toolConfig"] = tool_config;
            }
        }

        body
    }

    fn sign_and_build_request(
        &self,
        url_str: &str,
        body_bytes: &[u8],
    ) -> Result<reqwest::RequestBuilder, RayClawError> {
        let url: reqwest::Url = url_str
            .parse()
            .map_err(|e| RayClawError::LlmApi(format!("Invalid URL: {e}")))?;

        let now = chrono::Utc::now();
        let auth_headers = sign_request(
            "POST",
            &url,
            body_bytes,
            &self.credentials.region,
            "bedrock",
            &self.credentials.access_key_id,
            &self.credentials.secret_access_key,
            self.credentials.session_token.as_deref(),
            &now,
        );

        let mut builder = self
            .http
            .post(url_str)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body_bytes.to_vec());

        for (key, value) in auth_headers {
            builder = builder.header(&key, &value);
        }

        Ok(builder)
    }
}

#[async_trait]
impl LlmProvider for BedrockProvider {
    async fn send_message(
        &self,
        system: &str,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
    ) -> Result<MessagesResponse, RayClawError> {
        let messages = sanitize_messages(messages);
        let body = self.build_request_body(system, &messages, tools.as_deref());
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| RayClawError::LlmApi(format!("Failed to serialize request: {e}")))?;

        let url = self.converse_url();
        let mut retries = 0u32;
        let max_retries = 3;

        loop {
            let request = self.sign_and_build_request(&url, &body_bytes)?;
            let response = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    let classified = crate::error_classifier::classify_network(&e);
                    if let Some(delay) = crate::error_classifier::retry_delay(
                        classified.category,
                        retries,
                        max_retries,
                        None,
                    ) {
                        retries += 1;
                        warn!("Bedrock network error, retrying in {delay:?} (attempt {retries}/{max_retries}): {e}");
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(classified.into_error());
                }
            };
            let status = response.status();

            if status.is_success() {
                let response_body: serde_json::Value = response.json().await?;
                return Ok(translate_bedrock_response(&response_body));
            }

            let status_code = status.as_u16();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(crate::error_classifier::parse_retry_after);
            let err_body = response.text().await.unwrap_or_default();
            let mut classified = crate::error_classifier::classify_http(status_code, &err_body);
            classified.retry_after = retry_after;

            if let Some(delay) = crate::error_classifier::retry_delay(
                classified.category,
                retries,
                max_retries,
                classified.retry_after,
            ) {
                retries += 1;
                warn!(
                    "Bedrock {}, retrying in {delay:?} (attempt {retries}/{max_retries})",
                    classified
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            return Err(classified.into_error());
        }
    }

    async fn send_message_stream(
        &self,
        system: &str,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
        text_tx: Option<&UnboundedSender<String>>,
    ) -> Result<MessagesResponse, RayClawError> {
        let messages = sanitize_messages(messages);
        let body = self.build_request_body(system, &messages, tools.as_deref());
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| RayClawError::LlmApi(format!("Failed to serialize request: {e}")))?;

        let url = self.converse_stream_url();
        let url_parsed: reqwest::Url = url
            .parse()
            .map_err(|e| RayClawError::LlmApi(format!("Invalid URL: {e}")))?;

        let now = chrono::Utc::now();
        let auth_headers = sign_request(
            "POST",
            &url_parsed,
            &body_bytes,
            &self.credentials.region,
            "bedrock",
            &self.credentials.access_key_id,
            &self.credentials.secret_access_key,
            self.credentials.session_token.as_deref(),
            &now,
        );

        let mut builder = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body_bytes);

        for (key, value) in auth_headers {
            builder = builder.header(&key, &value);
        }

        let response = builder.send().await?;
        let status = response.status();

        if !status.is_success() {
            let err_body = response.text().await.unwrap_or_default();
            return Err(RayClawError::LlmApi(format!(
                "Bedrock ConverseStream HTTP {status}: {err_body}"
            )));
        }

        // Process event stream
        let mut parser = EventStreamParser::new();
        let mut stream = response.bytes_stream();

        let mut content_blocks: Vec<ResponseContentBlock> = Vec::new();
        let mut current_text = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_tool_input_json = String::new();
        let mut in_tool_use = false;
        let mut stop_reason: Option<String> = None;
        let mut usage: Option<Usage> = None;

        let idle_timeout = self.idle_timeout;
        loop {
            let chunk_result = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(res)) => res,
                Ok(None) => break,
                Err(_) => {
                    warn!("Bedrock streaming idle timeout after {:?}", idle_timeout);
                    return Err(RayClawError::LlmApi(format!(
                        "Streaming idle timeout: no data received for {:?}",
                        idle_timeout
                    )));
                }
            };
            let chunk = chunk_result?;
            parser.feed(&chunk);

            while let Some((event_type, payload)) = parser.next_frame() {
                match event_type.as_str() {
                    "contentBlockStart" => {
                        if let Some(block) = payload.get("start") {
                            if block.get("toolUse").is_some() {
                                // Flush any pending text
                                if !current_text.is_empty() {
                                    content_blocks.push(ResponseContentBlock::Text {
                                        text: std::mem::take(&mut current_text),
                                    });
                                }
                                let tool_use = block.get("toolUse").unwrap();
                                current_tool_id = tool_use
                                    .get("toolUseId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                current_tool_name = tool_use
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                current_tool_input_json.clear();
                                in_tool_use = true;
                            }
                        }
                    }
                    "contentBlockDelta" => {
                        if let Some(delta) = payload.get("delta") {
                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                current_text.push_str(text);
                                if let Some(tx) = text_tx {
                                    let _ = tx.send(text.to_string());
                                }
                            }
                            if let Some(json_chunk) = delta
                                .get("toolUse")
                                .and_then(|tu| tu.get("input").and_then(|i| i.as_str()))
                            {
                                current_tool_input_json.push_str(json_chunk);
                            }
                        }
                    }
                    "contentBlockStop" => {
                        if in_tool_use {
                            let input = serde_json::from_str(&current_tool_input_json)
                                .unwrap_or(serde_json::Value::Object(Default::default()));
                            content_blocks.push(ResponseContentBlock::ToolUse {
                                id: std::mem::take(&mut current_tool_id),
                                name: std::mem::take(&mut current_tool_name),
                                input,
                            });
                            current_tool_input_json.clear();
                            in_tool_use = false;
                        } else if !current_text.is_empty() {
                            content_blocks.push(ResponseContentBlock::Text {
                                text: std::mem::take(&mut current_text),
                            });
                        }
                    }
                    "messageStop" => {
                        stop_reason = payload
                            .get("stopReason")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                    "metadata" => {
                        usage = payload.get("usage").map(|u| Usage {
                            input_tokens: u.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0)
                                as u32,
                            output_tokens: u
                                .get("outputTokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32,
                        });
                    }
                    _ => {}
                }
            }
        }

        // Flush any remaining text
        if !current_text.is_empty() {
            content_blocks.push(ResponseContentBlock::Text { text: current_text });
        }

        Ok(MessagesResponse {
            content: content_blocks,
            stop_reason: normalize_stop_reason(stop_reason),
            usage,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_hex() {
        // Empty string SHA-256
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_sigv4_signing_key() {
        let key = sigv4_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        // AWS test vector — the derived signing key
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_sign_request_produces_authorization_header() {
        let url: reqwest::Url =
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/test/converse"
                .parse()
                .unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2025-01-15T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let headers = sign_request(
            "POST",
            &url,
            b"{}",
            "us-east-1",
            "bedrock",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            &now,
        );

        assert!(headers.iter().any(|(k, _)| k == "Authorization"));
        assert!(headers.iter().any(|(k, _)| k == "X-Amz-Date"));

        let auth = headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .unwrap()
            .1
            .clone();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/"));
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
    }

    #[test]
    fn test_sign_request_with_session_token() {
        let url: reqwest::Url =
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/test/converse"
                .parse()
                .unwrap();
        let now = chrono::Utc::now();

        let headers = sign_request(
            "POST",
            &url,
            b"{}",
            "us-west-2",
            "bedrock",
            "AKID",
            "SECRET",
            Some("session-token-123"),
            &now,
        );

        assert!(headers
            .iter()
            .any(|(k, v)| k == "X-Amz-Security-Token" && v == "session-token-123"));
    }

    #[test]
    fn test_translate_messages_text() {
        let messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Text("hello".into()),
        }];
        let result = translate_messages_to_bedrock(&messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn test_translate_messages_tool_use() {
        let messages = vec![Message {
            role: "assistant".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }]),
        }];
        let result = translate_messages_to_bedrock(&messages);
        let tool_use = &result[0]["content"][0]["toolUse"];
        assert_eq!(tool_use["toolUseId"], "tool-1");
        assert_eq!(tool_use["name"], "bash");
        assert_eq!(tool_use["input"]["command"], "ls");
    }

    #[test]
    fn test_translate_messages_tool_result() {
        let messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".into(),
                content: "output text".into(),
                is_error: Some(false),
            }]),
        }];
        let result = translate_messages_to_bedrock(&messages);
        let tool_result = &result[0]["content"][0]["toolResult"];
        assert_eq!(tool_result["toolUseId"], "tool-1");
        assert_eq!(tool_result["status"], "success");
    }

    #[test]
    fn test_translate_tools_to_bedrock() {
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run a command".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"],
            }),
        }];
        let config = translate_tools_to_bedrock(&tools);
        let spec = &config["tools"][0]["toolSpec"];
        assert_eq!(spec["name"], "bash");
        assert_eq!(spec["inputSchema"]["json"]["type"], "object");
    }

    #[test]
    fn test_translate_bedrock_response_text() {
        let body = serde_json::json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{ "text": "Hello!" }]
                }
            },
            "stopReason": "end_turn",
            "usage": { "inputTokens": 10, "outputTokens": 5 }
        });
        let resp = translate_bedrock_response(&body);
        assert_eq!(resp.content.len(), 1);
        if let ResponseContentBlock::Text { text } = &resp.content[0] {
            assert_eq!(text, "Hello!");
        } else {
            panic!("expected text block");
        }
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.usage.as_ref().unwrap().input_tokens, 10);
    }

    #[test]
    fn test_translate_bedrock_response_tool_use() {
        let body = serde_json::json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{
                        "toolUse": {
                            "toolUseId": "t1",
                            "name": "bash",
                            "input": { "command": "ls" }
                        }
                    }]
                }
            },
            "stopReason": "tool_use",
            "usage": { "inputTokens": 20, "outputTokens": 15 }
        });
        let resp = translate_bedrock_response(&body);
        if let ResponseContentBlock::ToolUse { id, name, input } = &resp.content[0] {
            assert_eq!(id, "t1");
            assert_eq!(name, "bash");
            assert_eq!(input["command"], "ls");
        } else {
            panic!("expected tool_use block");
        }
    }

    #[test]
    fn test_event_stream_parser_basic() {
        // Build a minimal event stream frame manually
        let event_type_name = b":event-type";
        let event_type_val = b"contentBlockDelta";
        let payload = b"{\"delta\":{\"text\":\"hi\"}}";

        // Build headers
        let mut headers = Vec::new();
        headers.push(event_type_name.len() as u8);
        headers.extend_from_slice(event_type_name);
        headers.push(7u8); // type = string
        headers.extend_from_slice(&(event_type_val.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type_val);

        let headers_len = headers.len() as u32;
        let total_len = 12 + headers_len + payload.len() as u32 + 4; // prelude(12) + headers + payload + msg_crc(4)

        let mut frame = Vec::new();
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.extend_from_slice(&headers_len.to_be_bytes());

        // Compute prelude CRC
        let prelude_crc = crc32fast::hash(&frame[..8]);
        frame.extend_from_slice(&prelude_crc.to_be_bytes());

        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);

        // Compute message CRC (over entire frame so far)
        let msg_crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&msg_crc.to_be_bytes());

        let mut parser = EventStreamParser::new();
        parser.feed(&frame);

        let (et, json) = parser.next_frame().expect("should parse frame");
        assert_eq!(et, "contentBlockDelta");
        assert_eq!(json["delta"]["text"], "hi");
    }

    #[test]
    fn test_mime_to_bedrock_format() {
        assert_eq!(mime_to_bedrock_format("image/png"), "png");
        assert_eq!(mime_to_bedrock_format("image/jpeg"), "jpeg");
        assert_eq!(mime_to_bedrock_format("image/gif"), "gif");
        assert_eq!(mime_to_bedrock_format("image/webp"), "webp");
        assert_eq!(mime_to_bedrock_format("image/bmp"), "jpeg"); // fallback
    }

    #[test]
    fn test_credentials_resolve_from_config() {
        let mut config = crate::config::Config {
            telegram_bot_token: "tok".into(),
            bot_username: "bot".into(),
            api_key: String::new(),
            llm_provider: "bedrock".into(),
            model: "anthropic.claude-sonnet-4-5-v2".into(),
            max_tokens: 8192,
            prompt_cache_ttl: "none".into(),
            max_tool_iterations: 50,
            max_loop_repeats: 3,
            llm_idle_timeout_secs: 30,
            max_history_messages: 50,
            llm_base_url: None,
            openai_api_key: None,
            allowed_groups: vec![],
            discord_bot_token: None,
            discord_allowed_channels: vec![],
            web_enabled: true,
            web_host: "127.0.0.1".into(),
            web_port: 3000,
            web_auth_token: None,
            web_max_inflight_per_session: 5,
            web_max_requests_per_window: 30,
            web_rate_window_seconds: 60,
            web_run_history_limit: 50,
            web_session_idle_ttl_seconds: 1800,
            max_document_size_mb: 100,
            memory_token_budget: 1500,
            max_session_messages: 50,
            compact_keep_recent: 10,
            show_thinking: false,
            data_dir: "./rayclaw.data".into(),
            working_dir: "./tmp".into(),
            working_dir_isolation: crate::config::WorkingDirIsolation::Chat,
            timezone: "UTC".into(),
            control_chat_ids: vec![],
            embedding_provider: None,
            embedding_api_key: None,
            embedding_base_url: None,
            embedding_model: None,
            embedding_dim: None,
            reflector_enabled: true,
            reflector_interval_mins: 15,
            model_prices: vec![],
            aws_region: Some("us-west-2".into()),
            aws_access_key_id: Some("AKID_TEST".into()),
            aws_secret_access_key: Some("SECRET_TEST".into()),
            aws_session_token: None,
            aws_profile: None,
            soul_path: None,
            skip_tool_approval: false,
            skills_dir: None,
            channels: std::collections::HashMap::new(),
        };
        config.channels.insert(
            "web".into(),
            serde_yaml::to_value(serde_json::json!({"enabled": true})).unwrap(),
        );

        let creds = AwsCredentials::resolve(&config).unwrap();
        assert_eq!(creds.access_key_id, "AKID_TEST");
        assert_eq!(creds.secret_access_key, "SECRET_TEST");
        assert_eq!(creds.region, "us-west-2");
        assert!(creds.session_token.is_none());
    }

    // -----------------------------------------------------------------------
    // Bedrock prompt caching
    // -----------------------------------------------------------------------

    fn make_bedrock_provider(cache_ttl: &str) -> BedrockProvider {
        BedrockProvider {
            http: reqwest::Client::new(),
            credentials: AwsCredentials {
                access_key_id: "AKID".into(),
                secret_access_key: "SECRET".into(),
                session_token: None,
                region: "us-east-1".into(),
            },
            model: "anthropic.claude-sonnet-4-5-v2".into(),
            max_tokens: 4096,
            prompt_cache_ttl: cache_ttl.into(),
            idle_timeout: std::time::Duration::from_secs(30),
        }
    }

    fn sample_tools() -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "bash".into(),
                description: "Run bash".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
            },
            ToolDefinition {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            },
        ]
    }

    #[test]
    fn test_build_request_body_bedrock_cache_disabled() {
        let provider = make_bedrock_provider("none");
        let msgs = vec![Message {
            role: "user".into(),
            content: MessageContent::Text("hi".into()),
        }];
        let tools = sample_tools();
        let body = provider.build_request_body("System prompt.", &msgs, Some(&tools));

        // System should have only text, no cachePoint
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], "System prompt.");
        assert!(sys[0].get("cachePoint").is_none());

        // toolConfig should have no cachePoint entry
        let tc_tools = body["toolConfig"]["tools"].as_array().unwrap();
        for t in tc_tools {
            assert!(t.get("cachePoint").is_none());
        }
    }

    #[test]
    fn test_build_request_body_bedrock_cache_5m() {
        let provider = make_bedrock_provider("5m");
        let msgs = vec![Message {
            role: "user".into(),
            content: MessageContent::Text("hi".into()),
        }];
        let tools = sample_tools();
        let body = provider.build_request_body("System prompt.", &msgs, Some(&tools));

        // System should have text + cachePoint
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert_eq!(sys[1]["cachePoint"]["type"], "default");
        assert_eq!(sys[1]["cachePoint"]["ttl"], "5m");

        // toolConfig should end with cachePoint
        let tc_tools = body["toolConfig"]["tools"].as_array().unwrap();
        let last = tc_tools.last().unwrap();
        assert_eq!(last["cachePoint"]["type"], "default");
        assert_eq!(last["cachePoint"]["ttl"], "5m");
    }

    #[test]
    fn test_build_request_body_bedrock_cache_1h() {
        let provider = make_bedrock_provider("1h");
        let msgs = vec![Message {
            role: "user".into(),
            content: MessageContent::Text("hi".into()),
        }];
        let tools = sample_tools();
        let body = provider.build_request_body("System prompt.", &msgs, Some(&tools));

        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[1]["cachePoint"]["ttl"], "1h");

        let tc_tools = body["toolConfig"]["tools"].as_array().unwrap();
        let last = tc_tools.last().unwrap();
        assert_eq!(last["cachePoint"]["ttl"], "1h");
    }

    #[test]
    fn test_build_request_body_bedrock_cache_no_tools() {
        let provider = make_bedrock_provider("5m");
        let msgs = vec![Message {
            role: "user".into(),
            content: MessageContent::Text("hi".into()),
        }];
        let body = provider.build_request_body("System prompt.", &msgs, None);

        // System should still get cachePoint
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert_eq!(sys[1]["cachePoint"]["type"], "default");
        assert_eq!(sys[1]["cachePoint"]["ttl"], "5m");

        // No toolConfig
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn test_build_request_body_bedrock_ttl_value_passed_through() {
        // Verify the actual TTL string is used, not hardcoded
        for ttl in &["5m", "1h", "30m", "2h"] {
            let provider = make_bedrock_provider(ttl);
            let msgs = vec![Message {
                role: "user".into(),
                content: MessageContent::Text("hi".into()),
            }];
            let tools = sample_tools();
            let body = provider.build_request_body("sys", &msgs, Some(&tools));

            let sys = body["system"].as_array().unwrap();
            assert_eq!(sys[1]["cachePoint"]["ttl"].as_str().unwrap(), *ttl);

            let tc_tools = body["toolConfig"]["tools"].as_array().unwrap();
            let last = tc_tools.last().unwrap();
            assert_eq!(last["cachePoint"]["ttl"].as_str().unwrap(), *ttl);
        }
    }
}
