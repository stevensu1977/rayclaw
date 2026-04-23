//! Phase 0 — Observability: session metrics, tool call tracking, and user feedback detection.

/// A single tool call recorded during an agent loop invocation.
#[derive(Debug, Clone)]
pub struct ToolCallMetric {
    pub tool_name: String,
    pub success: bool,
    pub duration_ms: u64,
    pub timestamp: String, // ISO 8601
}

/// Aggregated metrics for one `process_with_agent_impl` invocation.
#[derive(Debug, Clone)]
pub struct SessionMetrics {
    pub chat_id: i64,
    pub channel: String,
    pub timestamp: String,
    pub total_iterations: u32,
    pub tool_calls: Vec<ToolCallMetric>,
    pub llm_input_tokens: u64,
    pub llm_output_tokens: u64,
    pub error_count: u32,
    pub error_categories: Vec<String>,
    pub loop_detected: bool,
    pub overflow_recovered: bool,
    pub user_corrections: u32,
    pub user_positive_signals: u32,
    pub session_duration_ms: u64,
}

impl SessionMetrics {
    pub fn new(chat_id: i64, channel: &str) -> Self {
        Self {
            chat_id,
            channel: channel.to_string(),
            timestamp: String::new(),
            total_iterations: 0,
            tool_calls: Vec::new(),
            llm_input_tokens: 0,
            llm_output_tokens: 0,
            error_count: 0,
            error_categories: Vec::new(),
            loop_detected: false,
            overflow_recovered: false,
            user_corrections: 0,
            user_positive_signals: 0,
            session_duration_ms: 0,
        }
    }

    pub fn record_tool_call(&mut self, name: &str, success: bool, duration_ms: u64) {
        self.tool_calls.push(ToolCallMetric {
            tool_name: name.to_string(),
            success,
            duration_ms,
            timestamp: chrono::Utc::now().to_rfc3339(),
        });
    }

    pub fn record_llm_usage(&mut self, input_tokens: u32, output_tokens: u32) {
        self.llm_input_tokens += input_tokens as u64;
        self.llm_output_tokens += output_tokens as u64;
    }

    pub fn record_error(&mut self, category: &str) {
        self.error_count += 1;
        if !self.error_categories.contains(&category.to_string()) {
            self.error_categories.push(category.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// User feedback signal detection (keyword-based, no LLM)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackSignal {
    Correction,
    Positive,
}

const CORRECTION_KEYWORDS: &[&str] = &[
    "不对",
    "错了",
    "不是这样",
    "搞错",
    "wrong",
    "no that's not",
    "that's incorrect",
    "that's not right",
    "you're wrong",
    "not what i asked",
];

const POSITIVE_KEYWORDS: &[&str] = &[
    "谢谢",
    "太好了",
    "非常好",
    "perfect",
    "exactly",
    "thanks",
    "great job",
    "well done",
];

/// Scan a user message for feedback signals. Returns all detected signals.
pub fn detect_feedback_signals(text: &str) -> Vec<FeedbackSignal> {
    let lower = text.to_lowercase();
    let mut signals = Vec::new();

    if CORRECTION_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        signals.push(FeedbackSignal::Correction);
    }
    if POSITIVE_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        signals.push(FeedbackSignal::Positive);
    }

    signals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_correction_chinese() {
        let signals = detect_feedback_signals("不对，应该是另一个文件");
        assert!(signals.contains(&FeedbackSignal::Correction));
        assert!(!signals.contains(&FeedbackSignal::Positive));
    }

    #[test]
    fn test_detect_correction_english() {
        let signals = detect_feedback_signals("That's not right, try again");
        assert!(signals.contains(&FeedbackSignal::Correction));
    }

    #[test]
    fn test_detect_positive_chinese() {
        let signals = detect_feedback_signals("谢谢，太好了");
        assert!(signals.contains(&FeedbackSignal::Positive));
        assert!(!signals.contains(&FeedbackSignal::Correction));
    }

    #[test]
    fn test_detect_positive_english() {
        let signals = detect_feedback_signals("Perfect, exactly what I needed");
        assert!(signals.contains(&FeedbackSignal::Positive));
    }

    #[test]
    fn test_detect_both_signals() {
        // Edge case: message contains both correction and positive keywords
        let signals = detect_feedback_signals("不对，但谢谢你的尝试");
        assert!(signals.contains(&FeedbackSignal::Correction));
        assert!(signals.contains(&FeedbackSignal::Positive));
    }

    #[test]
    fn test_detect_no_signals() {
        let signals = detect_feedback_signals("请帮我查看这个文件");
        assert!(signals.is_empty());
    }

    #[test]
    fn test_detect_case_insensitive() {
        let signals = detect_feedback_signals("WRONG answer");
        assert!(signals.contains(&FeedbackSignal::Correction));
    }

    #[test]
    fn test_detect_empty_input() {
        let signals = detect_feedback_signals("");
        assert!(signals.is_empty());
    }

    #[test]
    fn test_session_metrics_record_tool_call() {
        let mut m = SessionMetrics::new(123, "telegram");
        m.record_tool_call("bash", true, 150);
        m.record_tool_call("read_file", false, 20);
        assert_eq!(m.tool_calls.len(), 2);
        assert!(m.tool_calls[0].success);
        assert!(!m.tool_calls[1].success);
    }

    #[test]
    fn test_session_metrics_record_llm_usage() {
        let mut m = SessionMetrics::new(123, "web");
        m.record_llm_usage(1000, 500);
        m.record_llm_usage(800, 300);
        assert_eq!(m.llm_input_tokens, 1800);
        assert_eq!(m.llm_output_tokens, 800);
    }

    #[test]
    fn test_session_metrics_record_error_dedup() {
        let mut m = SessionMetrics::new(123, "discord");
        m.record_error("context_overflow");
        m.record_error("tool_error");
        m.record_error("context_overflow"); // duplicate
        assert_eq!(m.error_count, 3);
        assert_eq!(m.error_categories.len(), 2);
    }
}
