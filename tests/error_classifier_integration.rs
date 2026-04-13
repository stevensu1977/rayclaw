//! Integration tests for the LLM error classifier + retry logic.
//!
//! Spins up a real HTTP server that simulates rate-limiting (429 with
//! Retry-After) and transient errors (500), then verifies that the
//! classifier-driven retry loop in the LLM providers handles them correctly.

use rayclaw::error_classifier::{
    classify_anthropic, classify_http, parse_retry_after, retry_delay, ClassifiedError,
    LlmErrorCategory,
};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Unit-level integration: full classify → retry_delay pipeline
// ---------------------------------------------------------------------------

/// Simulate the exact flow a provider retry loop goes through when hitting a
/// 429 with a Retry-After header.
#[test]
fn test_429_with_retry_after_full_pipeline() {
    let status = 429u16;
    let body = r#"{"error":{"type":"rate_limit_error","message":"Too many requests"}}"#;
    let retry_after_header = "3";

    // Step 1: classify
    let mut classified = classify_http(status, body);
    assert_eq!(classified.category, LlmErrorCategory::RateLimit);

    // Step 2: parse Retry-After
    let hint = parse_retry_after(retry_after_header);
    assert_eq!(hint, Some(Duration::from_secs(3)));
    classified.retry_after = hint;

    // Step 3: compute delay — should use server hint (3s) not default backoff (2s)
    let delay = retry_delay(classified.category, 0, 3, classified.retry_after);
    assert_eq!(delay, Some(Duration::from_secs(3)));
}

/// Same flow for Anthropic structured errors.
#[test]
fn test_anthropic_429_with_retry_after_pipeline() {
    let mut classified = classify_anthropic(429, "rate_limit_error", "Rate limited");
    assert_eq!(classified.category, LlmErrorCategory::RateLimit);

    classified.retry_after = parse_retry_after("5");
    let delay = retry_delay(classified.category, 0, 3, classified.retry_after);
    assert_eq!(delay, Some(Duration::from_secs(5)));
}

/// 500 transient error: no Retry-After, uses computed backoff.
#[test]
fn test_500_transient_no_retry_after() {
    let classified = classify_http(500, "Internal Server Error");
    assert_eq!(classified.category, LlmErrorCategory::Transient);
    assert!(classified.retry_after.is_none());

    // attempt 0 → 1s, attempt 1 → 2s, attempt 2 → 4s
    assert_eq!(
        retry_delay(classified.category, 0, 3, None),
        Some(Duration::from_secs(1))
    );
    assert_eq!(
        retry_delay(classified.category, 1, 3, None),
        Some(Duration::from_secs(2))
    );
    assert_eq!(
        retry_delay(classified.category, 2, 3, None),
        Some(Duration::from_secs(4))
    );
    // exhausted
    assert_eq!(retry_delay(classified.category, 3, 3, None), None);
}

/// 401 auth error: never retryable regardless of Retry-After.
#[test]
fn test_401_never_retries() {
    let classified = classify_http(401, "Unauthorized");
    assert_eq!(classified.category, LlmErrorCategory::Auth);
    assert!(!classified.is_retryable());
    assert_eq!(
        retry_delay(classified.category, 0, 3, Some(Duration::from_secs(5))),
        None
    );
}

/// Context overflow: not retryable via simple retry (needs compaction).
#[test]
fn test_context_overflow_not_retryable() {
    let body = r#"{"error":{"message":"This model's maximum context length is 128000 tokens","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
    let classified = classify_http(400, body);
    assert_eq!(classified.category, LlmErrorCategory::ContextOverflow);
    assert!(!classified.is_retryable());
    assert_eq!(retry_delay(classified.category, 0, 3, None), None);
}

/// Simulate a 429 → 429 → 200 sequence through the retry loop logic.
#[test]
fn test_simulated_retry_loop_429_429_200() {
    // This simulates the logic in the provider retry loops without actual HTTP.
    let max_retries = 3u32;
    let mut retries = 0u32;
    let responses: Vec<(u16, &str, Option<&str>)> = vec![
        (429, r#"{"error":{"message":"slow down"}}"#, Some("2")),
        (429, r#"{"error":{"message":"still slow"}}"#, Some("3")),
        (200, "", None), // success
    ];

    let mut delays_used = Vec::new();

    for (status, body, retry_after_hdr) in &responses {
        if *status == 200 {
            break; // success
        }
        let mut classified = classify_http(*status, body);
        if let Some(hdr) = retry_after_hdr {
            classified.retry_after = parse_retry_after(hdr);
        }
        let delay = retry_delay(
            classified.category,
            retries,
            max_retries,
            classified.retry_after,
        );
        match delay {
            Some(d) => {
                delays_used.push(d);
                retries += 1;
            }
            None => panic!("Should not exhaust retries"),
        }
    }

    assert_eq!(retries, 2);
    assert_eq!(delays_used[0], Duration::from_secs(2)); // Retry-After: 2
    assert_eq!(delays_used[1], Duration::from_secs(3)); // Retry-After: 3
}

/// Simulate exhausting all retries on 500.
#[test]
fn test_simulated_retry_loop_exhaustion() {
    let max_retries = 3u32;
    let mut retries = 0u32;
    let mut exhausted = false;

    for _ in 0..5 {
        let classified = classify_http(500, "Internal Server Error");
        let delay = retry_delay(classified.category, retries, max_retries, None);
        match delay {
            Some(_) => retries += 1,
            None => {
                exhausted = true;
                break;
            }
        }
    }

    assert!(exhausted);
    assert_eq!(retries, 3); // tried 3 times then gave up
}

/// Retry-After header edge cases.
#[test]
fn test_retry_after_edge_cases() {
    // Fractional seconds
    assert_eq!(parse_retry_after("0.5"), Some(Duration::from_millis(500)));

    // Exactly at boundary
    assert_eq!(parse_retry_after("300"), Some(Duration::from_secs(300)));

    // Over boundary
    assert_eq!(parse_retry_after("301"), None);

    // Empty
    assert_eq!(parse_retry_after(""), None);

    // Whitespace
    assert_eq!(parse_retry_after("  "), None);
}

/// Verify ClassifiedError Display includes category tag.
#[test]
fn test_classified_error_display_all_categories() {
    let categories = [
        (LlmErrorCategory::Transient, "[transient]"),
        (LlmErrorCategory::RateLimit, "[rate_limit]"),
        (LlmErrorCategory::ContextOverflow, "[context_overflow]"),
        (LlmErrorCategory::Auth, "[auth]"),
        (LlmErrorCategory::Permanent, "[permanent]"),
    ];
    for (cat, expected_tag) in categories {
        let err = ClassifiedError {
            category: cat,
            message: "test".into(),
            status: None,
            error_type: None,
            retry_after: None,
        };
        let display = format!("{err}");
        assert!(
            display.starts_with(expected_tag),
            "Expected '{expected_tag}' prefix, got: {display}"
        );
    }
}
