use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::agent_engine::archive_conversation;
use crate::agent_engine::process_with_agent_with_events;
use crate::agent_engine::AgentEvent;
use crate::agent_engine::AgentRequestContext;
use crate::channel::ConversationKind;
use crate::channel_adapter::ChannelAdapter;
use crate::db::call_blocking;
use crate::db::StoredMessage;
use crate::llm_types::Message as LlmMessage;
use crate::runtime::AppState;
use crate::usage::build_usage_report;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn default_base_url() -> String {
    "https://ilinkai.weixin.qq.com".into()
}

fn default_cdn_base_url() -> String {
    "https://novac2c.cdn.weixin.qq.com/c2c".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct WeixinChannelConfig {
    pub bot_token: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_cdn_base_url")]
    pub cdn_base_url: String,
    #[serde(default)]
    pub route_tag: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CHANNEL_VERSION: &str = env!("CARGO_PKG_VERSION");
const WEIXIN_TEXT_MAX_CHARS: usize = 4000;

// Message type constants
const MSG_TYPE_USER: u32 = 1;
const MSG_TYPE_BOT: u32 = 2;

// Message state constants
const MSG_STATE_NEW: u32 = 0;
#[allow(dead_code)]
const MSG_STATE_GENERATING: u32 = 1;
const MSG_STATE_FINISH: u32 = 2;

// Item type constants
const ITEM_TYPE_TEXT: u32 = 1;
const ITEM_TYPE_IMAGE: u32 = 2;
#[allow(dead_code)]
const ITEM_TYPE_VOICE: u32 = 3;
const ITEM_TYPE_FILE: u32 = 4;
const ITEM_TYPE_VIDEO: u32 = 5;

// Upload media type constants (for getuploadurl)
const UPLOAD_MEDIA_TYPE_IMAGE: u32 = 1;
const UPLOAD_MEDIA_TYPE_VIDEO: u32 = 2;
const UPLOAD_MEDIA_TYPE_FILE: u32 = 3;

// CDN upload retry
const CDN_UPLOAD_MAX_RETRIES: u32 = 3;
const CDN_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// Typing status
const TYPING_STATUS_TYPING: u32 = 1;
#[allow(dead_code)]
const TYPING_STATUS_CANCEL: u32 = 2;

// Timeouts
const LONG_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);
const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const CONFIG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// Session pause duration on errcode=-14
const SESSION_PAUSE_DURATION: std::time::Duration = std::time::Duration::from_secs(3600);

// ---------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BaseInfo {
    channel_version: String,
}

fn build_base_info() -> BaseInfo {
    BaseInfo {
        channel_version: CHANNEL_VERSION.to_string(),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct TextItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct CDNMedia {
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypt_query_param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aes_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypt_type: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct ImageItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    media: Option<CDNMedia>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mid_size: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct FileItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    media: Option<CDNMedia>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    len: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct VideoItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    media: Option<CDNMedia>,
    #[serde(skip_serializing_if = "Option::is_none")]
    video_size: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct MessageItem {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    item_type: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text_item: Option<TextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_item: Option<ImageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_item: Option<FileItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    video_item: Option<VideoItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    create_time_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct WeixinMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_type: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_state: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_list: Option<Vec<MessageItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct SendMessageReq {
    msg: WeixinMessage,
    base_info: BaseInfo,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
struct GetUpdatesResp {
    #[serde(default)]
    ret: Option<i32>,
    #[serde(default)]
    errcode: Option<i32>,
    #[serde(default)]
    errmsg: Option<String>,
    #[serde(default)]
    msgs: Option<Vec<WeixinMessage>>,
    #[serde(default)]
    get_updates_buf: Option<String>,
    #[serde(default)]
    longpolling_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
struct GetConfigResp {
    #[serde(default)]
    ret: Option<i32>,
    #[serde(default)]
    errmsg: Option<String>,
    #[serde(default)]
    typing_ticket: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
struct GetUploadUrlResp {
    #[serde(default)]
    upload_param: Option<String>,
    #[serde(default)]
    thumb_upload_param: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct SendTypingReq {
    ilink_user_id: String,
    typing_ticket: String,
    status: u32,
    base_info: BaseInfo,
}

// ---------------------------------------------------------------------------
// Sync buffer persistence
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SyncBuf {
    #[serde(default)]
    get_updates_buf: String,
}

fn sync_buf_path(data_dir: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(data_dir).join("weixin_sync.json")
}

fn load_sync_buf(data_dir: &str) -> String {
    let path = sync_buf_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            if let Ok(buf) = serde_json::from_str::<SyncBuf>(&contents) {
                buf.get_updates_buf
            } else {
                String::new()
            }
        }
        Err(_) => String::new(),
    }
}

fn save_sync_buf(data_dir: &str, buf: &str) {
    let path = sync_buf_path(data_dir);
    let sync = SyncBuf {
        get_updates_buf: buf.to_string(),
    };
    if let Ok(json) = serde_json::to_string(&sync) {
        let _ = std::fs::write(&path, json);
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Generate X-WECHAT-UIN header: random u32 -> decimal string -> base64.
fn random_wechat_uin() -> String {
    use base64::Engine;
    // Use uuid v4 bytes as a source of randomness (rand crate not available)
    let uuid_bytes = uuid::Uuid::new_v4();
    let val = u32::from_le_bytes([
        uuid_bytes.as_bytes()[0],
        uuid_bytes.as_bytes()[1],
        uuid_bytes.as_bytes()[2],
        uuid_bytes.as_bytes()[3],
    ]);
    let decimal = val.to_string();
    base64::engine::general_purpose::STANDARD.encode(decimal.as_bytes())
}

/// Build common headers for all Weixin API requests.
fn build_headers(token: &str, route_tag: Option<&str>) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    headers.insert("AuthorizationType", "ilink_bot_token".parse().unwrap());
    if !token.is_empty() {
        if let Ok(val) = format!("Bearer {token}").parse() {
            headers.insert(reqwest::header::AUTHORIZATION, val);
        }
    }
    if let Ok(val) = random_wechat_uin().parse() {
        headers.insert("X-WECHAT-UIN", val);
    }
    if let Some(tag) = route_tag {
        if !tag.is_empty() {
            if let Ok(val) = tag.parse() {
                headers.insert("SKRouteTag", val);
            }
        }
    }
    headers
}

fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

// ---------------------------------------------------------------------------
// API functions
// ---------------------------------------------------------------------------

async fn api_get_updates(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    route_tag: Option<&str>,
    get_updates_buf: &str,
) -> Result<GetUpdatesResp, String> {
    let url = format!("{}ilink/bot/getupdates", ensure_trailing_slash(base_url));
    let body = serde_json::json!({
        "get_updates_buf": get_updates_buf,
        "base_info": build_base_info(),
    });

    let resp = client
        .post(&url)
        .headers(build_headers(token, route_tag))
        .json(&body)
        .timeout(LONG_POLL_TIMEOUT)
        .send()
        .await;

    match resp {
        Ok(r) => {
            let text = r
                .text()
                .await
                .map_err(|e| format!("getUpdates: failed to read body: {e}"))?;
            serde_json::from_str(&text)
                .map_err(|e| format!("getUpdates: failed to parse response: {e}"))
        }
        Err(e) if e.is_timeout() => {
            // Long-poll timeout is normal; return empty response
            Ok(GetUpdatesResp {
                ret: Some(0),
                errcode: None,
                errmsg: None,
                msgs: Some(Vec::new()),
                get_updates_buf: Some(get_updates_buf.to_string()),
                longpolling_timeout_ms: None,
            })
        }
        Err(e) => Err(format!("getUpdates: request failed: {e}")),
    }
}

async fn api_send_message(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    route_tag: Option<&str>,
    msg: WeixinMessage,
) -> Result<(), String> {
    let url = format!("{}ilink/bot/sendmessage", ensure_trailing_slash(base_url));
    let req = SendMessageReq {
        msg,
        base_info: build_base_info(),
    };

    let resp = client
        .post(&url)
        .headers(build_headers(token, route_tag))
        .json(&req)
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("sendMessage: request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("sendMessage: HTTP {status}: {body}"));
    }
    Ok(())
}

async fn api_get_config(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    route_tag: Option<&str>,
    ilink_user_id: &str,
    context_token: Option<&str>,
) -> Result<GetConfigResp, String> {
    let url = format!("{}ilink/bot/getconfig", ensure_trailing_slash(base_url));
    let mut body = serde_json::json!({
        "ilink_user_id": ilink_user_id,
        "base_info": build_base_info(),
    });
    if let Some(ct) = context_token {
        body["context_token"] = serde_json::Value::String(ct.to_string());
    }

    let resp = client
        .post(&url)
        .headers(build_headers(token, route_tag))
        .json(&body)
        .timeout(CONFIG_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("getConfig: request failed: {e}"))?;

    let text = resp
        .text()
        .await
        .map_err(|e| format!("getConfig: failed to read body: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("getConfig: failed to parse response: {e}"))
}

async fn api_send_typing(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    route_tag: Option<&str>,
    ilink_user_id: &str,
    typing_ticket: &str,
) -> Result<(), String> {
    let url = format!("{}ilink/bot/sendtyping", ensure_trailing_slash(base_url));
    let req = SendTypingReq {
        ilink_user_id: ilink_user_id.to_string(),
        typing_ticket: typing_ticket.to_string(),
        status: TYPING_STATUS_TYPING,
        base_info: build_base_info(),
    };

    let _ = client
        .post(&url)
        .headers(build_headers(token, route_tag))
        .json(&req)
        .timeout(CONFIG_TIMEOUT)
        .send()
        .await;

    Ok(())
}

/// Get a pre-signed CDN upload URL for a file.
#[allow(clippy::too_many_arguments)]
async fn api_get_upload_url(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    route_tag: Option<&str>,
    filekey: &str,
    media_type: u32,
    to_user_id: &str,
    rawsize: u64,
    rawfilemd5: &str,
    filesize: u64,
    aeskey_hex: &str,
) -> Result<GetUploadUrlResp, String> {
    let url = format!("{}ilink/bot/getuploadurl", ensure_trailing_slash(base_url));
    let body = serde_json::json!({
        "filekey": filekey,
        "media_type": media_type,
        "to_user_id": to_user_id,
        "rawsize": rawsize,
        "rawfilemd5": rawfilemd5,
        "filesize": filesize,
        "no_need_thumb": true,
        "aeskey": aeskey_hex,
        "base_info": build_base_info(),
    });

    let resp = client
        .post(&url)
        .headers(build_headers(token, route_tag))
        .json(&body)
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("getUploadUrl: request failed: {e}"))?;

    let text = resp
        .text()
        .await
        .map_err(|e| format!("getUploadUrl: failed to read body: {e}"))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("getUploadUrl: failed to parse response: {e}: {text}"))
}

// ---------------------------------------------------------------------------
// AES-128-ECB encryption for CDN upload
// ---------------------------------------------------------------------------

/// Encrypt plaintext with AES-128-ECB and PKCS7 padding.
fn aes_ecb_encrypt(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    use aes::Aes128;
    use ecb::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyInit};

    type Aes128EcbEnc = ecb::Encryptor<Aes128>;
    let enc = Aes128EcbEnc::new(key.into());
    enc.encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

/// Compute AES-128-ECB ciphertext size with PKCS7 padding.
fn aes_ecb_padded_size(plaintext_size: u64) -> u64 {
    (plaintext_size + 1).div_ceil(16) * 16
}

// ---------------------------------------------------------------------------
// CDN upload pipeline
// ---------------------------------------------------------------------------

/// Result of a CDN upload.
#[allow(dead_code)]
struct CdnUploadResult {
    filekey: String,
    download_encrypted_query_param: String,
    aeskey_hex: String,
    file_size: u64,
    file_size_ciphertext: u64,
}

/// Upload a file to the Weixin CDN: getUploadUrl → encrypt → POST to CDN.
#[allow(clippy::too_many_arguments)]
async fn upload_file_to_cdn(
    client: &reqwest::Client,
    base_url: &str,
    cdn_base_url: &str,
    token: &str,
    route_tag: Option<&str>,
    to_user_id: &str,
    plaintext: &[u8],
    media_type: u32,
) -> Result<CdnUploadResult, String> {
    use md5::{Digest, Md5};

    let rawsize = plaintext.len() as u64;
    let rawfilemd5 = format!("{:x}", Md5::new().chain_update(plaintext).finalize());
    let filesize = aes_ecb_padded_size(rawsize);

    // Generate random filekey (32 hex chars) and AES key (16 bytes)
    let filekey_uuid = uuid::Uuid::new_v4();
    let filekey = format!("{:032x}", filekey_uuid.as_u128());
    let aeskey_bytes: [u8; 16] = {
        let u = uuid::Uuid::new_v4();
        let b = u.as_bytes();
        let mut k = [0u8; 16];
        k.copy_from_slice(b);
        k
    };
    let aeskey_hex = hex::encode(aeskey_bytes);

    info!(
        "Weixin CDN: uploading filekey={} rawsize={} filesize={} media_type={}",
        filekey, rawsize, filesize, media_type
    );

    // Step 1: Get upload URL
    let upload_resp = api_get_upload_url(
        client,
        base_url,
        token,
        route_tag,
        &filekey,
        media_type,
        to_user_id,
        rawsize,
        &rawfilemd5,
        filesize,
        &aeskey_hex,
    )
    .await?;

    let upload_param = upload_resp
        .upload_param
        .ok_or("getUploadUrl: no upload_param in response")?;

    // Step 2: Encrypt with AES-128-ECB
    let ciphertext = aes_ecb_encrypt(plaintext, &aeskey_bytes);

    // Step 3: Upload ciphertext to CDN
    let cdn_url = format!(
        "{}/upload?encrypted_query_param={}&filekey={}",
        cdn_base_url,
        urlencoding::encode(&upload_param),
        urlencoding::encode(&filekey),
    );

    let mut download_param: Option<String> = None;
    let mut last_error: Option<String> = None;

    for attempt in 1..=CDN_UPLOAD_MAX_RETRIES {
        let result = client
            .post(&cdn_url)
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext.clone())
            .timeout(CDN_UPLOAD_TIMEOUT)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() >= 400 && status.as_u16() < 500 {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(format!("CDN upload client error (HTTP {status}): {body}"));
                }
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    last_error = Some(format!("CDN upload server error (HTTP {status}): {body}"));
                    warn!(
                        "Weixin CDN: attempt {attempt}/{CDN_UPLOAD_MAX_RETRIES} failed: {}",
                        last_error.as_deref().unwrap_or("unknown")
                    );
                    continue;
                }
                // Extract download param from response header
                if let Some(val) = resp.headers().get("x-encrypted-param") {
                    download_param = val.to_str().ok().map(|s| s.to_string());
                }
                if download_param.is_none() {
                    last_error = Some("CDN upload: missing x-encrypted-param header".into());
                    warn!(
                        "Weixin CDN: attempt {attempt}/{CDN_UPLOAD_MAX_RETRIES}: {}",
                        last_error.as_deref().unwrap_or("unknown")
                    );
                    continue;
                }
                break;
            }
            Err(e) => {
                last_error = Some(format!("CDN upload request failed: {e}"));
                warn!(
                    "Weixin CDN: attempt {attempt}/{CDN_UPLOAD_MAX_RETRIES}: {}",
                    last_error.as_deref().unwrap_or("unknown")
                );
            }
        }
    }

    let download_encrypted_query_param =
        download_param.ok_or_else(|| last_error.unwrap_or_else(|| "CDN upload failed".into()))?;

    info!("Weixin CDN: upload success filekey={}", filekey);

    Ok(CdnUploadResult {
        filekey,
        download_encrypted_query_param,
        aeskey_hex,
        file_size: rawsize,
        file_size_ciphertext: filesize,
    })
}

/// Build a CDNMedia reference from upload result.
fn build_cdn_media(upload: &CdnUploadResult) -> CDNMedia {
    use base64::Engine;
    CDNMedia {
        encrypt_query_param: Some(upload.download_encrypted_query_param.clone()),
        aes_key: Some(
            base64::engine::general_purpose::STANDARD.encode(upload.aeskey_hex.as_bytes()),
        ),
        encrypt_type: Some(1),
    }
}

/// Determine the upload media type from a file extension.
fn media_type_for_extension(ext: &str) -> (u32, u32) {
    // Returns (upload_media_type, item_type)
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" => {
            (UPLOAD_MEDIA_TYPE_IMAGE, ITEM_TYPE_IMAGE)
        }
        "mp4" | "mov" | "avi" | "mkv" | "webm" => (UPLOAD_MEDIA_TYPE_VIDEO, ITEM_TYPE_VIDEO),
        _ => (UPLOAD_MEDIA_TYPE_FILE, ITEM_TYPE_FILE),
    }
}

// ---------------------------------------------------------------------------
// Text splitting
// ---------------------------------------------------------------------------

fn split_text_chunks(text: &str, max_chars: usize) -> Vec<&str> {
    if text.len() <= max_chars {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        if start + max_chars >= text.len() {
            chunks.push(&text[start..]);
            break;
        }

        let end = start + max_chars;
        // Find char boundary
        let end = if text.is_char_boundary(end) {
            end
        } else {
            (start..end)
                .rev()
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(start)
        };

        // Try to split at newline
        let search_region = &text[start..end];
        let split_at = search_region
            .rfind('\n')
            .map(|pos| start + pos + 1)
            .unwrap_or(end);

        if split_at <= start {
            // Force split forward
            let forced = (end..=text.len())
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(text.len());
            chunks.push(&text[start..forced]);
            start = forced;
        } else {
            chunks.push(&text[start..split_at]);
            start = split_at;
        }
    }

    chunks
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

pub struct WeixinAdapter {
    http_client: reqwest::Client,
    base_url: String,
    cdn_base_url: String,
    bot_token: String,
    route_tag: Option<String>,
    /// from_user_id -> context_token (must echo in every outbound send)
    context_tokens: Arc<RwLock<HashMap<String, String>>>,
    /// from_user_id -> typing_ticket
    typing_tickets: Arc<RwLock<HashMap<String, String>>>,
}

impl WeixinAdapter {
    pub fn new(config: &WeixinChannelConfig) -> Self {
        WeixinAdapter {
            http_client: reqwest::Client::new(),
            base_url: config.base_url.clone(),
            cdn_base_url: config.cdn_base_url.clone(),
            bot_token: config.bot_token.clone(),
            route_tag: config.route_tag.clone(),
            context_tokens: Arc::new(RwLock::new(HashMap::new())),
            typing_tickets: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Store a context_token for a user.
    pub async fn set_context_token(&self, user_id: &str, token: &str) {
        self.context_tokens
            .write()
            .await
            .insert(user_id.to_string(), token.to_string());
    }

    /// Get cached context_token for a user.
    pub async fn get_context_token(&self, user_id: &str) -> Option<String> {
        self.context_tokens.read().await.get(user_id).cloned()
    }

    /// Store a typing_ticket for a user.
    pub async fn set_typing_ticket(&self, user_id: &str, ticket: &str) {
        self.typing_tickets
            .write()
            .await
            .insert(user_id.to_string(), ticket.to_string());
    }

    /// Get cached typing_ticket for a user.
    pub async fn get_typing_ticket(&self, user_id: &str) -> Option<String> {
        self.typing_tickets.read().await.get(user_id).cloned()
    }

    /// Send a text reply to a user, splitting long messages.
    async fn send_text_to_user(
        &self,
        to_user_id: &str,
        text: &str,
        context_token: Option<&str>,
    ) -> Result<(), String> {
        if context_token.is_none() {
            warn!("Weixin: sending without context_token to {to_user_id} — reply may be orphaned");
        }

        for chunk in split_text_chunks(text, WEIXIN_TEXT_MAX_CHARS) {
            let msg = WeixinMessage {
                from_user_id: Some(String::new()),
                to_user_id: Some(to_user_id.to_string()),
                client_id: Some(uuid::Uuid::new_v4().to_string()),
                message_type: Some(MSG_TYPE_BOT),
                message_state: Some(MSG_STATE_FINISH),
                item_list: Some(vec![MessageItem {
                    item_type: Some(ITEM_TYPE_TEXT),
                    text_item: Some(TextItem {
                        text: Some(chunk.to_string()),
                    }),
                    ..Default::default()
                }]),
                context_token: context_token.map(|s| s.to_string()),
                ..Default::default()
            };

            api_send_message(
                &self.http_client,
                &self.base_url,
                &self.bot_token,
                self.route_tag.as_deref(),
                msg,
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for WeixinAdapter {
    fn name(&self) -> &str {
        "weixin"
    }

    fn chat_type_routes(&self) -> Vec<(&str, ConversationKind)> {
        vec![("weixin_dm", ConversationKind::Private)]
    }

    async fn send_text(&self, external_chat_id: &str, text: &str) -> Result<(), String> {
        let context_token = self.get_context_token(external_chat_id).await;
        self.send_text_to_user(external_chat_id, text, context_token.as_deref())
            .await
    }

    async fn send_attachment(
        &self,
        external_chat_id: &str,
        file_path: &Path,
        caption: Option<&str>,
    ) -> Result<String, String> {
        let context_token = self.get_context_token(external_chat_id).await;

        // Read file
        let plaintext = tokio::fs::read(file_path)
            .await
            .map_err(|e| format!("Failed to read attachment: {e}"))?;

        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment.bin")
            .to_string();

        let ext = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let (upload_media_type, item_type) = media_type_for_extension(&ext);

        info!(
            "Weixin: sending attachment {} ({} bytes) type={} to {}",
            filename,
            plaintext.len(),
            item_type,
            external_chat_id
        );

        // Upload to CDN
        let upload = upload_file_to_cdn(
            &self.http_client,
            &self.base_url,
            &self.cdn_base_url,
            &self.bot_token,
            self.route_tag.as_deref(),
            external_chat_id,
            &plaintext,
            upload_media_type,
        )
        .await?;

        // Send caption as separate text message first (if provided)
        if let Some(cap) = caption {
            if !cap.is_empty() {
                let text_msg = WeixinMessage {
                    from_user_id: Some(String::new()),
                    to_user_id: Some(external_chat_id.to_string()),
                    client_id: Some(uuid::Uuid::new_v4().to_string()),
                    message_type: Some(MSG_TYPE_BOT),
                    message_state: Some(MSG_STATE_FINISH),
                    item_list: Some(vec![MessageItem {
                        item_type: Some(ITEM_TYPE_TEXT),
                        text_item: Some(TextItem {
                            text: Some(cap.to_string()),
                        }),
                        ..Default::default()
                    }]),
                    context_token: context_token.clone(),
                    ..Default::default()
                };
                api_send_message(
                    &self.http_client,
                    &self.base_url,
                    &self.bot_token,
                    self.route_tag.as_deref(),
                    text_msg,
                )
                .await?;
            }
        }

        // Build media message item based on type
        let media_item = match item_type {
            ITEM_TYPE_IMAGE => MessageItem {
                item_type: Some(ITEM_TYPE_IMAGE),
                image_item: Some(ImageItem {
                    media: Some(build_cdn_media(&upload)),
                    mid_size: Some(upload.file_size_ciphertext),
                }),
                ..Default::default()
            },
            ITEM_TYPE_VIDEO => MessageItem {
                item_type: Some(ITEM_TYPE_VIDEO),
                video_item: Some(VideoItem {
                    media: Some(build_cdn_media(&upload)),
                    video_size: Some(upload.file_size_ciphertext),
                }),
                ..Default::default()
            },
            _ => MessageItem {
                item_type: Some(ITEM_TYPE_FILE),
                file_item: Some(FileItem {
                    media: Some(build_cdn_media(&upload)),
                    file_name: Some(filename.clone()),
                    len: Some(upload.file_size.to_string()),
                }),
                ..Default::default()
            },
        };

        // Send the media message
        let media_msg = WeixinMessage {
            from_user_id: Some(String::new()),
            to_user_id: Some(external_chat_id.to_string()),
            client_id: Some(uuid::Uuid::new_v4().to_string()),
            message_type: Some(MSG_TYPE_BOT),
            message_state: Some(MSG_STATE_FINISH),
            item_list: Some(vec![media_item]),
            context_token: context_token.clone(),
            ..Default::default()
        };

        api_send_message(
            &self.http_client,
            &self.base_url,
            &self.bot_token,
            self.route_tag.as_deref(),
            media_msg,
        )
        .await?;

        let content = match caption {
            Some(c) if !c.is_empty() => format!("[attachment:{}] {}", file_path.display(), c),
            _ => format!("[attachment:{}]", file_path.display()),
        };

        Ok(content)
    }
}

// ---------------------------------------------------------------------------
// Message handler
// ---------------------------------------------------------------------------

/// Extract text content from a WeixinMessage's item_list.
fn extract_text(msg: &WeixinMessage) -> String {
    let items = match &msg.item_list {
        Some(items) => items,
        None => return String::new(),
    };
    for item in items {
        if item.item_type == Some(ITEM_TYPE_TEXT) {
            if let Some(ref ti) = item.text_item {
                if let Some(ref text) = ti.text {
                    return text.clone();
                }
            }
        }
    }
    String::new()
}

#[allow(clippy::too_many_arguments)]
async fn handle_weixin_message(
    app_state: Arc<AppState>,
    adapter: Arc<WeixinAdapter>,
    from_user_id: String,
    text: String,
    message_id: String,
) {
    let chat_type = "weixin_dm";
    let title = format!("weixin-{from_user_id}");

    let chat_id = call_blocking(app_state.db.clone(), {
        let external = from_user_id.clone();
        let title = title.clone();
        let chat_type = chat_type.to_string();
        move |db| db.resolve_or_create_chat_id("weixin", &external, Some(&title), &chat_type)
    })
    .await
    .unwrap_or(0);

    if chat_id == 0 {
        error!("Weixin: failed to resolve chat ID for {from_user_id}");
        return;
    }

    // Store incoming message
    let stored = StoredMessage {
        id: if message_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            message_id.clone()
        },
        chat_id,
        sender_name: from_user_id.clone(),
        content: text.clone(),
        is_from_bot: false,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let _ = call_blocking(app_state.db.clone(), move |db| db.store_message(&stored)).await;

    // Handle slash commands
    let trimmed = text.trim();
    if trimmed == "/reset" {
        let _ = call_blocking(app_state.db.clone(), move |db| {
            db.clear_chat_context(chat_id)
        })
        .await;
        let _ = adapter
            .send_text(&from_user_id, "Context cleared (session + chat history).")
            .await;
        return;
    }
    if trimmed == "/skills" {
        let formatted = app_state.skills.list_skills_formatted();
        let _ = adapter.send_text(&from_user_id, &formatted).await;
        return;
    }
    if trimmed == "/archive" {
        if let Ok(Some((json, _))) =
            call_blocking(app_state.db.clone(), move |db| db.load_session(chat_id)).await
        {
            let messages: Vec<LlmMessage> = serde_json::from_str(&json).unwrap_or_default();
            if messages.is_empty() {
                let _ = adapter
                    .send_text(&from_user_id, "No session to archive.")
                    .await;
            } else {
                archive_conversation(&app_state.config.data_dir, "weixin", chat_id, &messages);
                let _ = adapter
                    .send_text(
                        &from_user_id,
                        &format!("Archived {} messages.", messages.len()),
                    )
                    .await;
            }
        } else {
            let _ = adapter
                .send_text(&from_user_id, "No session to archive.")
                .await;
        }
        return;
    }
    if trimmed == "/usage" {
        match build_usage_report(app_state.db.clone(), &app_state.config, chat_id).await {
            Ok(report) => {
                let _ = adapter.send_text(&from_user_id, &report).await;
            }
            Err(e) => {
                let _ = adapter
                    .send_text(
                        &from_user_id,
                        &format!("Failed to query usage statistics: {e}"),
                    )
                    .await;
            }
        }
        return;
    }

    info!(
        "Weixin message from {} : {}",
        from_user_id,
        text.chars().take(100).collect::<String>()
    );

    // Start typing indicator
    let typing_adapter = adapter.clone();
    let typing_user = from_user_id.clone();
    let typing_handle = tokio::spawn(async move {
        // Try to get typing ticket
        let ticket = typing_adapter.get_typing_ticket(&typing_user).await;
        if let Some(ticket) = ticket {
            loop {
                let _ = api_send_typing(
                    &typing_adapter.http_client,
                    &typing_adapter.base_url,
                    &typing_adapter.bot_token,
                    typing_adapter.route_tag.as_deref(),
                    &typing_user,
                    &ticket,
                )
                .await;
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }
        }
    });

    // Call agent engine
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

    match process_with_agent_with_events(
        &app_state,
        AgentRequestContext {
            caller_channel: "weixin",
            chat_id,
            chat_type: "private",
        },
        None,
        None,
        Some(&event_tx),
    )
    .await
    {
        Ok(response) => {
            drop(event_tx);
            // Check if send_message tool was used
            let mut used_send_message_tool = false;
            while let Some(evt) = event_rx.recv().await {
                if let AgentEvent::ToolStart { name } = evt {
                    if name == "send_message" {
                        used_send_message_tool = true;
                    }
                }
            }

            // Stop typing
            typing_handle.abort();

            if !response.is_empty() && !used_send_message_tool {
                if let Err(e) = adapter.send_text(&from_user_id, &response).await {
                    error!("Weixin: failed to send response to {from_user_id}: {e}");
                }
            }

            // Store bot response
            if !response.is_empty() {
                let bot_msg = StoredMessage {
                    id: uuid::Uuid::new_v4().to_string(),
                    chat_id,
                    sender_name: app_state.config.bot_username.clone(),
                    content: response,
                    is_from_bot: true,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                };
                let _ =
                    call_blocking(app_state.db.clone(), move |db| db.store_message(&bot_msg)).await;
            }
        }
        Err(e) => {
            typing_handle.abort();
            error!("Weixin: agent error for {from_user_id}: {e}");
            let _ = adapter
                .send_text(&from_user_id, &format!("Sorry, an error occurred: {e}"))
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Long-poll message loop
// ---------------------------------------------------------------------------

pub async fn start_weixin_bot(app_state: Arc<AppState>, adapter: Arc<WeixinAdapter>) {
    let weixin_cfg: WeixinChannelConfig = match app_state.config.channel_config("weixin") {
        Some(c) => c,
        None => {
            error!("Weixin channel not configured");
            return;
        }
    };

    if weixin_cfg.bot_token.trim().is_empty() {
        error!("Weixin: bot_token is empty");
        return;
    }

    info!("Weixin: starting long-poll message loop");

    let http_client = &adapter.http_client;
    let mut get_updates_buf = load_sync_buf(&app_state.config.data_dir);
    let mut paused_until: Option<std::time::Instant> = None;

    info!(
        "Weixin: long-poll loop started, base_url={}",
        weixin_cfg.base_url
    );

    loop {
        // Check session pause
        if let Some(until) = paused_until {
            if std::time::Instant::now() < until {
                let remaining = until - std::time::Instant::now();
                info!("Weixin: session paused, {}s remaining", remaining.as_secs());
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            } else {
                info!("Weixin: session pause expired, resuming");
                paused_until = None;
            }
        }

        // Long-poll for updates
        let resp = api_get_updates(
            http_client,
            &weixin_cfg.base_url,
            &weixin_cfg.bot_token,
            weixin_cfg.route_tag.as_deref(),
            &get_updates_buf,
        )
        .await;

        match resp {
            Ok(data) => {
                // Check for errcode=-14 (session expired)
                if data.errcode == Some(-14) {
                    warn!("Weixin: session expired (errcode=-14), pausing for 1 hour");
                    paused_until = Some(std::time::Instant::now() + SESSION_PAUSE_DURATION);
                    continue;
                }

                // Update sync buf
                if let Some(ref buf) = data.get_updates_buf {
                    if !buf.is_empty() {
                        get_updates_buf = buf.clone();
                        save_sync_buf(&app_state.config.data_dir, &get_updates_buf);
                    }
                }

                // Process messages
                if let Some(msgs) = data.msgs {
                    for msg in msgs {
                        // Only process user messages (type=1) with state NEW (0) or FINISH (2)
                        let msg_type = msg.message_type.unwrap_or(0);
                        let msg_state = msg.message_state.unwrap_or(0);
                        if msg_type != MSG_TYPE_USER {
                            continue;
                        }
                        // Accept NEW and FINISH states (skip GENERATING)
                        if msg_state != MSG_STATE_NEW && msg_state != MSG_STATE_FINISH {
                            continue;
                        }

                        let from_user_id = match &msg.from_user_id {
                            Some(id) if !id.is_empty() => id.clone(),
                            _ => continue,
                        };

                        let text = extract_text(&msg);
                        if text.trim().is_empty() {
                            continue;
                        }

                        // Cache context_token
                        if let Some(ref ct) = msg.context_token {
                            adapter.set_context_token(&from_user_id, ct).await;
                        }

                        // Fetch typing ticket if we don't have one
                        if adapter.get_typing_ticket(&from_user_id).await.is_none() {
                            match api_get_config(
                                http_client,
                                &weixin_cfg.base_url,
                                &weixin_cfg.bot_token,
                                weixin_cfg.route_tag.as_deref(),
                                &from_user_id,
                                msg.context_token.as_deref(),
                            )
                            .await
                            {
                                Ok(config_resp) => {
                                    if let Some(ticket) = config_resp.typing_ticket {
                                        adapter.set_typing_ticket(&from_user_id, &ticket).await;
                                    }
                                }
                                Err(e) => {
                                    warn!("Weixin: failed to get config for {from_user_id}: {e}");
                                }
                            }
                        }

                        let message_id = msg
                            .message_id
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

                        // Spawn message handler
                        let state = app_state.clone();
                        let adapter_clone = adapter.clone();
                        tokio::spawn(async move {
                            handle_weixin_message(
                                state,
                                adapter_clone,
                                from_user_id,
                                text,
                                message_id,
                            )
                            .await;
                        });
                    }
                }
            }
            Err(e) => {
                warn!("Weixin: getUpdates error: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// QR code login flow
// ---------------------------------------------------------------------------

const DEFAULT_API_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const QR_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);
const QR_MAX_REFRESH: u32 = 3;
const QR_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(480);

#[derive(Debug, serde::Deserialize)]
struct QrCodeResponse {
    qrcode: Option<String>,
    qrcode_img_content: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct QrStatusResponse {
    status: Option<String>,
    bot_token: Option<String>,
    ilink_bot_id: Option<String>,
    baseurl: Option<String>,
    ilink_user_id: Option<String>,
}

/// Credential file saved after successful QR login.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct WeixinCredentials {
    pub bot_token: String,
    pub base_url: String,
    pub account_id: String,
    pub user_id: String,
    pub saved_at: String,
}

fn credentials_path(data_dir: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(data_dir).join("weixin_credentials.json")
}

fn save_credentials(data_dir: &str, creds: &WeixinCredentials) -> Result<(), String> {
    let path = credentials_path(data_dir);
    let json =
        serde_json::to_string_pretty(creds).map_err(|e| format!("serialize credentials: {e}"))?;
    std::fs::write(&path, &json)
        .map_err(|e| format!("write credentials to {}: {e}", path.display()))?;
    info!("Weixin credentials saved to {}", path.display());
    Ok(())
}

pub fn load_credentials(data_dir: &str) -> Option<WeixinCredentials> {
    let path = credentials_path(data_dir);
    let contents = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Fetch a QR code from the Weixin iLink API.
async fn fetch_qr_code(
    client: &reqwest::Client,
    base_url: &str,
    route_tag: Option<&str>,
) -> Result<QrCodeResponse, String> {
    let url = format!(
        "{}ilink/bot/get_bot_qrcode?bot_type=3",
        ensure_trailing_slash(base_url)
    );
    let mut req = client.get(&url);
    if let Some(tag) = route_tag {
        if !tag.is_empty() {
            req = req.header("SKRouteTag", tag);
        }
    }
    let resp = req
        .timeout(CONFIG_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("fetch QR code: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("fetch QR code: HTTP {status}: {body}"));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("fetch QR code: read body: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("fetch QR code: parse: {e}"))
}

/// Long-poll QR code scan status.
async fn poll_qr_status(
    client: &reqwest::Client,
    base_url: &str,
    qrcode: &str,
    route_tag: Option<&str>,
) -> Result<QrStatusResponse, String> {
    let url = format!(
        "{}ilink/bot/get_qrcode_status?qrcode={}",
        ensure_trailing_slash(base_url),
        urlencoding::encode(qrcode)
    );
    let mut req = client.get(&url).header("iLink-App-ClientVersion", "1");
    if let Some(tag) = route_tag {
        if !tag.is_empty() {
            req = req.header("SKRouteTag", tag);
        }
    }
    let resp = req.timeout(QR_POLL_TIMEOUT).send().await;
    match resp {
        Ok(r) => {
            let text = r
                .text()
                .await
                .map_err(|e| format!("poll QR status: read body: {e}"))?;
            serde_json::from_str(&text).map_err(|e| format!("poll QR status: parse: {e}"))
        }
        Err(e) if e.is_timeout() => Ok(QrStatusResponse {
            status: Some("wait".into()),
            bot_token: None,
            ilink_bot_id: None,
            baseurl: None,
            ilink_user_id: None,
        }),
        Err(e) => Err(format!("poll QR status: {e}")),
    }
}

/// Print a QR code to the terminal using Unicode block characters.
fn print_qr_terminal(data: &str) {
    use qrcode::QrCode;

    let code = match QrCode::new(data) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  Failed to generate QR code: {e}");
            eprintln!("  Open this URL in a browser instead:");
            eprintln!("  {data}");
            return;
        }
    };

    let matrix = code.to_colors();
    let width = code.width();

    // Use Unicode half-block chars to pack 2 rows into 1 terminal line.
    // Each module = 1 character wide for a compact QR code.
    println!();

    let border = "  ".to_string() + &"█".repeat(width + 2);
    println!("{border}");

    let rows: Vec<&[qrcode::Color]> = matrix.chunks(width).collect();
    let mut y = 0;
    while y < rows.len() {
        let top_row = rows[y];
        let bot_row = if y + 1 < rows.len() {
            Some(rows[y + 1])
        } else {
            None
        };

        let mut line = String::from("  █"); // left border
        for x in 0..width {
            let top = top_row[x] == qrcode::Color::Dark;
            let bot = bot_row
                .map(|r| r[x] == qrcode::Color::Dark)
                .unwrap_or(false);
            match (top, bot) {
                (false, false) => line.push('█'), // both light
                (true, true) => line.push(' '),   // both dark
                (true, false) => line.push('▄'),  // top dark, bot light
                (false, true) => line.push('▀'),  // top light, bot dark
            }
        }
        line.push('█'); // right border
        println!("{line}");
        y += 2;
    }

    println!("{border}");
    println!();
}

/// Update the bot_token in an existing rayclaw.config.yaml file.
/// Preserves the rest of the YAML content by doing a targeted string replacement.
fn write_token_to_config(config_path: &str, creds: &WeixinCredentials) -> Result<(), String> {
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| format!("read config {config_path}: {e}"))?;

    // Check if weixin section already exists
    let new_content = if content.contains("channels:") && content.contains("weixin:") {
        // Replace existing bot_token line under weixin
        let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        let mut in_weixin = false;
        for line in &mut lines {
            let trimmed = line.trim();
            // Detect we're inside channels.weixin section
            if trimmed.starts_with("weixin:") {
                in_weixin = true;
                continue;
            }
            if in_weixin {
                if trimmed.starts_with("bot_token:") {
                    // Find indentation
                    let indent = line.len() - line.trim_start().len();
                    let spaces = &line[..indent];
                    *line = format!("{spaces}bot_token: \"{}\"", creds.bot_token);
                    in_weixin = false;
                } else if !trimmed.is_empty()
                    && !trimmed.starts_with('#')
                    && !trimmed.starts_with("base_url:")
                    && !trimmed.starts_with("route_tag:")
                    && !trimmed.starts_with("account_id:")
                    && !trimmed.starts_with("bot_token:")
                    && !line.starts_with(' ')
                    && !line.starts_with('\t')
                {
                    // Left the weixin block without finding bot_token
                    in_weixin = false;
                }
            }
        }
        lines.join("\n") + "\n"
    } else if content.contains("channels:") {
        // channels: exists but no weixin section — append it
        let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        // Find the channels: line and insert weixin after the last channel block
        if let Some(idx) = lines.iter().position(|l| l.trim() == "channels:") {
            // Find insertion point: after channels: and its children
            let mut insert_at = idx + 1;
            while insert_at < lines.len() {
                let l = &lines[insert_at];
                if l.trim().is_empty()
                    || l.starts_with(' ')
                    || l.starts_with('\t')
                    || l.trim().starts_with('#')
                {
                    insert_at += 1;
                } else {
                    break;
                }
            }
            lines.insert(insert_at, "  weixin:".to_string());
            lines.insert(
                insert_at + 1,
                format!("    bot_token: \"{}\"", creds.bot_token),
            );
            if creds.base_url != DEFAULT_API_BASE_URL {
                lines.insert(
                    insert_at + 2,
                    format!("    base_url: \"{}\"", creds.base_url),
                );
            }
        }
        lines.join("\n") + "\n"
    } else {
        // No channels section at all — append both
        let mut addition = format!(
            "\nchannels:\n  weixin:\n    bot_token: \"{}\"",
            creds.bot_token
        );
        if creds.base_url != DEFAULT_API_BASE_URL {
            addition.push_str(&format!("\n    base_url: \"{}\"", creds.base_url));
        }
        addition.push('\n');
        content + &addition
    };

    std::fs::write(config_path, &new_content)
        .map_err(|e| format!("write config {config_path}: {e}"))?;
    Ok(())
}

/// Interactive QR login flow. Returns credentials on success.
/// If `config_path` is provided, auto-writes the bot_token into the config file.
pub async fn run_qr_login(
    base_url: Option<&str>,
    route_tag: Option<&str>,
    data_dir: &str,
    config_path: Option<&str>,
) -> Result<WeixinCredentials, String> {
    let base_url = base_url.unwrap_or(DEFAULT_API_BASE_URL);
    let client = reqwest::Client::new();

    println!("WeChat iLink Bot — QR Code Login");
    println!("================================\n");
    println!("Base URL: {base_url}");
    println!("Fetching QR code...\n");

    let qr = fetch_qr_code(&client, base_url, route_tag).await?;
    let qrcode = qr.qrcode.ok_or("Server returned empty qrcode field")?;
    let qrcode_url = qr.qrcode_img_content.unwrap_or_else(|| qrcode.clone());

    print_qr_terminal(&qrcode_url);

    let deadline = std::time::Instant::now() + QR_LOGIN_TIMEOUT;
    let mut current_qrcode = qrcode;
    let mut refresh_count: u32 = 1;
    let mut scanned_printed = false;

    println!(
        "Waiting for scan (timeout: {}s)...\n",
        QR_LOGIN_TIMEOUT.as_secs()
    );

    while std::time::Instant::now() < deadline {
        let status = poll_qr_status(&client, base_url, &current_qrcode, route_tag).await?;

        match status.status.as_deref().unwrap_or("wait") {
            "wait" => {
                eprint!(".");
            }
            "scaned" => {
                if !scanned_printed {
                    eprintln!("\n\n  Scanned! Please confirm on your phone...");
                    scanned_printed = true;
                }
            }
            "expired" => {
                refresh_count += 1;
                if refresh_count > QR_MAX_REFRESH {
                    return Err("QR code expired too many times. Please try again.".into());
                }
                eprintln!(
                    "\n\n  QR code expired, refreshing ({refresh_count}/{QR_MAX_REFRESH})..."
                );
                let qr = fetch_qr_code(&client, base_url, route_tag).await?;
                current_qrcode = qr.qrcode.ok_or("Server returned empty qrcode on refresh")?;
                let url = qr
                    .qrcode_img_content
                    .unwrap_or_else(|| current_qrcode.clone());
                print_qr_terminal(&url);
                scanned_printed = false;
            }
            "confirmed" => {
                let bot_token = status
                    .bot_token
                    .ok_or("Login confirmed but bot_token missing")?;
                let account_id = status
                    .ilink_bot_id
                    .ok_or("Login confirmed but ilink_bot_id missing")?;
                let login_base_url = status.baseurl.unwrap_or_else(|| base_url.to_string());
                let user_id = status.ilink_user_id.unwrap_or_default();

                let creds = WeixinCredentials {
                    bot_token,
                    base_url: login_base_url,
                    account_id,
                    user_id,
                    saved_at: chrono::Utc::now().to_rfc3339(),
                };

                save_credentials(data_dir, &creds)?;

                println!("\n\n  Login successful!");
                println!("  Account ID: {}", creds.account_id);
                println!("  Base URL:   {}", creds.base_url);
                println!(
                    "  Token:      {}...{}",
                    &creds.bot_token[..8.min(creds.bot_token.len())],
                    if creds.bot_token.len() > 12 {
                        &creds.bot_token[creds.bot_token.len() - 4..]
                    } else {
                        ""
                    }
                );
                println!(
                    "\n  Credentials saved to {}/weixin_credentials.json",
                    data_dir
                );

                if let Some(cfg_path) = config_path {
                    match write_token_to_config(cfg_path, &creds) {
                        Ok(()) => {
                            println!("  Config updated: {cfg_path}");
                        }
                        Err(e) => {
                            eprintln!("  Warning: failed to update config: {e}");
                            println!("\n  Add manually to your rayclaw.config.yaml:");
                            println!("    channels:");
                            println!("      weixin:");
                            println!("        bot_token: \"{}\"", creds.bot_token);
                        }
                    }
                } else {
                    println!("\n  Add to your rayclaw.config.yaml:");
                    println!("    channels:");
                    println!("      weixin:");
                    println!("        bot_token: \"{}\"", creds.bot_token);
                    if creds.base_url != DEFAULT_API_BASE_URL {
                        println!("        base_url: \"{}\"", creds.base_url);
                    }
                }

                return Ok(creds);
            }
            other => {
                warn!("Weixin QR login: unknown status: {other}");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Err("Login timed out. Please try again.".into())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- aes_ecb_padded_size --------------------------------------------------

    #[test]
    fn test_aes_ecb_padded_size_zero() {
        // 0 bytes plaintext → 1 byte padding minimum → ceil((0+1)/16)*16 = 16
        assert_eq!(aes_ecb_padded_size(0), 16);
    }

    #[test]
    fn test_aes_ecb_padded_size_15() {
        // 15 bytes → ceil((15+1)/16)*16 = 16
        assert_eq!(aes_ecb_padded_size(15), 16);
    }

    #[test]
    fn test_aes_ecb_padded_size_16() {
        // 16 bytes → ceil((16+1)/16)*16 = 32  (needs extra block for padding)
        assert_eq!(aes_ecb_padded_size(16), 32);
    }

    #[test]
    fn test_aes_ecb_padded_size_31() {
        // 31 bytes → ceil((31+1)/16)*16 = 32
        assert_eq!(aes_ecb_padded_size(31), 32);
    }

    #[test]
    fn test_aes_ecb_padded_size_100() {
        // 100 bytes → ceil((100+1)/16)*16 = ceil(101/16)*16 = 7*16 = 112
        assert_eq!(aes_ecb_padded_size(100), 112);
    }

    // -- aes_ecb_encrypt / decrypt round-trip ---------------------------------

    #[test]
    fn test_aes_ecb_encrypt_basic() {
        let key: [u8; 16] = [0x42; 16];
        let plaintext = b"hello world!";
        let ciphertext = aes_ecb_encrypt(plaintext, &key);

        // Ciphertext length must match padded size
        assert_eq!(
            ciphertext.len() as u64,
            aes_ecb_padded_size(plaintext.len() as u64)
        );
        // Must not be all zeros
        assert!(ciphertext.iter().any(|&b| b != 0));
        // Must differ from plaintext
        assert_ne!(&ciphertext[..plaintext.len()], plaintext);
    }

    #[test]
    fn test_aes_ecb_encrypt_deterministic() {
        let key: [u8; 16] = [0xAB; 16];
        let plaintext = b"same input same output";
        let ct1 = aes_ecb_encrypt(plaintext, &key);
        let ct2 = aes_ecb_encrypt(plaintext, &key);
        assert_eq!(ct1, ct2);
    }

    #[test]
    fn test_aes_ecb_encrypt_different_keys() {
        let key1: [u8; 16] = [0x01; 16];
        let key2: [u8; 16] = [0x02; 16];
        let plaintext = b"test data for encryption";
        let ct1 = aes_ecb_encrypt(plaintext, &key1);
        let ct2 = aes_ecb_encrypt(plaintext, &key2);
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn test_aes_ecb_encrypt_empty() {
        let key: [u8; 16] = [0xFF; 16];
        let ciphertext = aes_ecb_encrypt(b"", &key);
        // Empty plaintext → 16 bytes ciphertext (one block of padding)
        assert_eq!(ciphertext.len(), 16);
    }

    #[test]
    fn test_aes_ecb_encrypt_large_file() {
        let key: [u8; 16] = [0x55; 16];
        let plaintext = vec![0xAA; 4096]; // 4KB
        let ciphertext = aes_ecb_encrypt(&plaintext, &key);
        assert_eq!(ciphertext.len() as u64, aes_ecb_padded_size(4096));
    }

    // -- media_type_for_extension ---------------------------------------------

    #[test]
    fn test_media_type_image_extensions() {
        for ext in ["png", "jpg", "jpeg", "gif", "bmp", "webp"] {
            let (upload, item) = media_type_for_extension(ext);
            assert_eq!(upload, UPLOAD_MEDIA_TYPE_IMAGE, "ext={ext}");
            assert_eq!(item, ITEM_TYPE_IMAGE, "ext={ext}");
        }
    }

    #[test]
    fn test_media_type_video_extensions() {
        for ext in ["mp4", "mov", "avi", "mkv", "webm"] {
            let (upload, item) = media_type_for_extension(ext);
            assert_eq!(upload, UPLOAD_MEDIA_TYPE_VIDEO, "ext={ext}");
            assert_eq!(item, ITEM_TYPE_VIDEO, "ext={ext}");
        }
    }

    #[test]
    fn test_media_type_file_extensions() {
        for ext in [
            "pdf", "md", "doc", "docx", "zip", "txt", "xls", "ppt", "csv", "",
        ] {
            let (upload, item) = media_type_for_extension(ext);
            assert_eq!(upload, UPLOAD_MEDIA_TYPE_FILE, "ext={ext}");
            assert_eq!(item, ITEM_TYPE_FILE, "ext={ext}");
        }
    }

    // -- build_cdn_media ------------------------------------------------------

    #[test]
    fn test_build_cdn_media() {
        let upload = CdnUploadResult {
            filekey: "abc123".into(),
            download_encrypted_query_param: "some_param".into(),
            aeskey_hex: "00112233445566778899aabbccddeeff".into(),
            file_size: 1024,
            file_size_ciphertext: 1040,
        };
        let media = build_cdn_media(&upload);
        assert_eq!(media.encrypt_query_param.as_deref(), Some("some_param"));
        assert_eq!(media.encrypt_type, Some(1));
        // aes_key should be base64 of the hex string bytes
        assert!(media.aes_key.is_some());
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            media.aes_key.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "00112233445566778899aabbccddeeff"
        );
    }

    // -- split_text_chunks ----------------------------------------------------

    #[test]
    fn test_split_text_short() {
        let chunks = split_text_chunks("hello", 4000);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn test_split_text_exact_boundary() {
        let text = "a".repeat(4000);
        let chunks = split_text_chunks(&text, 4000);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_split_text_over_boundary() {
        let text = "a".repeat(4001);
        let chunks = split_text_chunks(&text, 4000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4000);
        assert_eq!(chunks[1].len(), 1);
    }
}
