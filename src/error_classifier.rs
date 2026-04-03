//! LLM error classifier — categorises API errors so the caller can decide
//! whether to retry, back off, or give up.

use std::time::Duration;

/// Broad error categories that drive retry / abort decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmErrorCategory {
    /// Transient server-side error (5xx, network blip). Safe to retry.
    Transient,
    /// Rate-limited (429). Back off then retry.
    RateLimit,
    /// Context window exceeded. Needs compaction, not a simple retry.
    ContextOverflow,
    /// Authentication / permission failure. Retrying won't help.
    Auth,
    /// Bad request, invalid model, malformed input. Permanent.
    Permanent,
}

/// A classified error with enough context for the retry loop.
#[derive(Debug, Clone)]
pub struct ClassifiedError {
    pub category: LlmErrorCategory,
    /// Human-readable message (from the provider or synthesised).
    pub message: String,
    /// HTTP status code, if available.
    pub status: Option<u16>,
    /// Provider-specific error type string (e.g. "overloaded_error").
    pub error_type: Option<String>,
}

impl std::fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = match self.category {
            LlmErrorCategory::Transient => "transient",
            LlmErrorCategory::RateLimit => "rate_limit",
            LlmErrorCategory::ContextOverflow => "context_overflow",
            LlmErrorCategory::Auth => "auth",
            LlmErrorCategory::Permanent => "permanent",
        };
        write!(f, "[{tag}] {}", self.message)
    }
}

impl ClassifiedError {
    /// Whether the caller should retry this error (possibly after a delay).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.category,
            LlmErrorCategory::Transient | LlmErrorCategory::RateLimit
        )
    }
}

// ---------------------------------------------------------------------------
// Retry policy
// ---------------------------------------------------------------------------

/// Decides the delay for a given retry attempt. Returns `None` when retries
/// are exhausted.
pub fn retry_delay(category: LlmErrorCategory, attempt: u32, max_retries: u32) -> Option<Duration> {
    if attempt >= max_retries {
        return None;
    }
    match category {
        LlmErrorCategory::RateLimit => {
            // Longer backoff for rate-limits: 2, 4, 8, 16 …
            Some(Duration::from_secs(2u64.pow(attempt + 1)))
        }
        LlmErrorCategory::Transient => {
            // Shorter backoff for transient: 1, 2, 4 …
            Some(Duration::from_secs(2u64.pow(attempt)))
        }
        _ => None, // non-retryable
    }
}

// ---------------------------------------------------------------------------
// Classification from HTTP status + optional body
// ---------------------------------------------------------------------------

/// Classify an HTTP error response. Works for all providers.
pub fn classify_http(status: u16, body: &str) -> ClassifiedError {
    // --- rate limit ---
    if status == 429 {
        return ClassifiedError {
            category: LlmErrorCategory::RateLimit,
            message: extract_message(body).unwrap_or_else(|| "Rate limited".into()),
            status: Some(status),
            error_type: Some("rate_limit_error".into()),
        };
    }

    // --- auth ---
    if status == 401 || status == 403 {
        return ClassifiedError {
            category: LlmErrorCategory::Auth,
            message: extract_message(body)
                .unwrap_or_else(|| format!("Authentication error (HTTP {status})")),
            status: Some(status),
            error_type: Some("authentication_error".into()),
        };
    }

    // --- server errors (transient) ---
    if status == 500 || status == 502 || status == 503 || status == 529 {
        return ClassifiedError {
            category: LlmErrorCategory::Transient,
            message: extract_message(body)
                .unwrap_or_else(|| format!("Server error (HTTP {status})")),
            status: Some(status),
            error_type: None,
        };
    }

    // 504 gateway timeout — transient
    if status == 504 {
        return ClassifiedError {
            category: LlmErrorCategory::Transient,
            message: "Gateway timeout".into(),
            status: Some(status),
            error_type: None,
        };
    }

    // --- context overflow heuristics (from body) ---
    if is_context_overflow(body) {
        return ClassifiedError {
            category: LlmErrorCategory::ContextOverflow,
            message: extract_message(body).unwrap_or_else(|| "Context length exceeded".into()),
            status: Some(status),
            error_type: Some("context_overflow".into()),
        };
    }

    // --- everything else is permanent ---
    ClassifiedError {
        category: LlmErrorCategory::Permanent,
        message: extract_message(body).unwrap_or_else(|| format!("API error (HTTP {status})")),
        status: Some(status),
        error_type: None,
    }
}

/// Classify an Anthropic-specific error using the structured error body.
pub fn classify_anthropic(status: u16, error_type: &str, message: &str) -> ClassifiedError {
    // Anthropic error types: https://docs.anthropic.com/en/api/errors
    let category = match error_type {
        "rate_limit_error" => LlmErrorCategory::RateLimit,
        "authentication_error" | "permission_error" => LlmErrorCategory::Auth,
        "overloaded_error" | "api_error" => LlmErrorCategory::Transient,
        "invalid_request_error" if is_context_overflow(message) => {
            LlmErrorCategory::ContextOverflow
        }
        _ => {
            // Fall back to HTTP-status heuristic
            if status == 429 {
                LlmErrorCategory::RateLimit
            } else if status >= 500 {
                LlmErrorCategory::Transient
            } else {
                LlmErrorCategory::Permanent
            }
        }
    };
    ClassifiedError {
        category,
        message: format!("{error_type}: {message}"),
        status: Some(status),
        error_type: Some(error_type.to_string()),
    }
}

/// Classify a network / connection error (no HTTP response at all).
pub fn classify_network(err: &reqwest::Error) -> ClassifiedError {
    ClassifiedError {
        category: LlmErrorCategory::Transient,
        message: format!("Network error: {err}"),
        status: None,
        error_type: None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether an error body mentions context-length overflow.
fn is_context_overflow(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("context length")
        || lower.contains("maximum context")
        || lower.contains("token limit")
        || lower.contains("too many tokens")
        || lower.contains("prompt is too long")
        || lower.contains("max_tokens") && (lower.contains("exceed") || lower.contains("too long"))
}

/// Try to extract a human-readable message from a JSON error body.
fn extract_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    // Anthropic: { "error": { "message": "..." } }
    if let Some(msg) = v.pointer("/error/message").and_then(|m| m.as_str()) {
        return Some(msg.to_string());
    }
    // OpenAI: { "error": { "message": "..." } }  (same shape)
    // Bedrock / generic: { "message": "..." }
    if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
        return Some(msg.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_429_rate_limit() {
        let c = classify_http(429, r#"{"error":{"message":"rate limited"}}"#);
        assert_eq!(c.category, LlmErrorCategory::RateLimit);
        assert!(c.is_retryable());
    }

    #[test]
    fn test_classify_500_transient() {
        let c = classify_http(500, "Internal Server Error");
        assert_eq!(c.category, LlmErrorCategory::Transient);
        assert!(c.is_retryable());
    }

    #[test]
    fn test_classify_502_transient() {
        let c = classify_http(502, "Bad Gateway");
        assert_eq!(c.category, LlmErrorCategory::Transient);
    }

    #[test]
    fn test_classify_503_transient() {
        let c = classify_http(503, "Service Unavailable");
        assert_eq!(c.category, LlmErrorCategory::Transient);
    }

    #[test]
    fn test_classify_504_transient() {
        let c = classify_http(504, "Gateway Timeout");
        assert_eq!(c.category, LlmErrorCategory::Transient);
    }

    #[test]
    fn test_classify_529_transient() {
        // Anthropic uses 529 for overloaded
        let c = classify_http(529, "Overloaded");
        assert_eq!(c.category, LlmErrorCategory::Transient);
    }

    #[test]
    fn test_classify_401_auth() {
        let c = classify_http(401, "Unauthorized");
        assert_eq!(c.category, LlmErrorCategory::Auth);
        assert!(!c.is_retryable());
    }

    #[test]
    fn test_classify_403_auth() {
        let c = classify_http(403, "Forbidden");
        assert_eq!(c.category, LlmErrorCategory::Auth);
    }

    #[test]
    fn test_classify_400_permanent() {
        let c = classify_http(400, r#"{"error":{"message":"invalid model"}}"#);
        assert_eq!(c.category, LlmErrorCategory::Permanent);
        assert!(!c.is_retryable());
        assert_eq!(c.message, "invalid model");
    }

    #[test]
    fn test_classify_context_overflow_openai() {
        let body = r#"{"error":{"message":"This model's maximum context length is 128000 tokens","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        let c = classify_http(400, body);
        assert_eq!(c.category, LlmErrorCategory::ContextOverflow);
    }

    #[test]
    fn test_classify_context_overflow_anthropic() {
        let c = classify_anthropic(
            400,
            "invalid_request_error",
            "prompt is too long: 130000 tokens > 128000 maximum",
        );
        assert_eq!(c.category, LlmErrorCategory::ContextOverflow);
    }

    #[test]
    fn test_classify_anthropic_overloaded() {
        let c = classify_anthropic(529, "overloaded_error", "Overloaded");
        assert_eq!(c.category, LlmErrorCategory::Transient);
    }

    #[test]
    fn test_classify_anthropic_rate_limit() {
        let c = classify_anthropic(429, "rate_limit_error", "Rate limited");
        assert_eq!(c.category, LlmErrorCategory::RateLimit);
    }

    #[test]
    fn test_classify_anthropic_auth() {
        let c = classify_anthropic(401, "authentication_error", "Invalid API key");
        assert_eq!(c.category, LlmErrorCategory::Auth);
    }

    #[test]
    fn test_classify_anthropic_permission() {
        let c = classify_anthropic(403, "permission_error", "Not allowed");
        assert_eq!(c.category, LlmErrorCategory::Auth);
    }

    #[test]
    fn test_retry_delay_rate_limit() {
        assert_eq!(
            retry_delay(LlmErrorCategory::RateLimit, 0, 3),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            retry_delay(LlmErrorCategory::RateLimit, 1, 3),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            retry_delay(LlmErrorCategory::RateLimit, 2, 3),
            Some(Duration::from_secs(8))
        );
        assert_eq!(retry_delay(LlmErrorCategory::RateLimit, 3, 3), None); // exhausted
    }

    #[test]
    fn test_retry_delay_transient() {
        assert_eq!(
            retry_delay(LlmErrorCategory::Transient, 0, 3),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            retry_delay(LlmErrorCategory::Transient, 1, 3),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            retry_delay(LlmErrorCategory::Transient, 2, 3),
            Some(Duration::from_secs(4))
        );
        assert_eq!(retry_delay(LlmErrorCategory::Transient, 3, 3), None);
    }

    #[test]
    fn test_retry_delay_permanent_never() {
        assert_eq!(retry_delay(LlmErrorCategory::Permanent, 0, 3), None);
    }

    #[test]
    fn test_retry_delay_auth_never() {
        assert_eq!(retry_delay(LlmErrorCategory::Auth, 0, 3), None);
    }

    #[tokio::test]
    async fn test_classify_network_error() {
        // Trigger a real connection-refused error
        let err = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .unwrap()
            .get("http://127.0.0.1:1")
            .send()
            .await
            .unwrap_err();
        let c = classify_network(&err);
        assert_eq!(c.category, LlmErrorCategory::Transient);
        assert!(c.status.is_none());
    }

    #[test]
    fn test_extract_message_anthropic() {
        let body = r#"{"error":{"type":"rate_limit_error","message":"Too many requests"}}"#;
        let msg = extract_message(body);
        assert_eq!(msg.unwrap(), "Too many requests");
    }

    #[test]
    fn test_extract_message_generic() {
        let body = r#"{"message":"something went wrong"}"#;
        let msg = extract_message(body);
        assert_eq!(msg.unwrap(), "something went wrong");
    }

    #[test]
    fn test_extract_message_non_json() {
        let msg = extract_message("plain text error");
        assert!(msg.is_none());
    }

    #[test]
    fn test_display_format() {
        let c = ClassifiedError {
            category: LlmErrorCategory::RateLimit,
            message: "slow down".into(),
            status: Some(429),
            error_type: None,
        };
        assert_eq!(format!("{c}"), "[rate_limit] slow down");
    }
}
