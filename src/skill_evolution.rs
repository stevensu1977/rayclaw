//! Phase 1 — Skill Evolution: pattern detection, auto-generation, promotion, archival, learnings.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use tracing::error;

use crate::db::{SkillHealthRow, ToolCallSequence};
use crate::llm::LlmProvider;
use crate::llm_types::{Message, MessageContent, ResponseContentBlock};

// ---------------------------------------------------------------------------
// Tool pattern detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ToolPattern {
    pub sequence: Vec<String>,
    pub occurrences: usize,
    pub hash: String,
    pub example_chat_ids: Vec<i64>,
}

/// Extract n-grams of tool names from a session's tool call list.
fn extract_ngrams(tools: &[String], min_len: usize) -> Vec<Vec<String>> {
    let mut ngrams = Vec::new();
    for n in min_len..=tools.len().min(8) {
        for window in tools.windows(n) {
            ngrams.push(window.to_vec());
        }
    }
    ngrams
}

/// Hash a tool name sequence for dedup.
fn hash_sequence(seq: &[String]) -> String {
    seq.join("|")
}

/// Detect repeated tool-call patterns from per-session sequences.
pub fn detect_patterns(
    sequences: &[ToolCallSequence],
    min_seq_len: usize,
    min_occurrences: usize,
) -> Vec<ToolPattern> {
    // Count occurrences of each n-gram across all sessions
    let mut counts: HashMap<String, (Vec<String>, Vec<i64>)> = HashMap::new();

    for seq in sequences {
        let ngrams = extract_ngrams(&seq.tool_names, min_seq_len);
        // Deduplicate within a single session
        let mut seen_in_session = std::collections::HashSet::new();
        for ngram in ngrams {
            let h = hash_sequence(&ngram);
            if seen_in_session.insert(h.clone()) {
                let entry = counts.entry(h).or_insert_with(|| (ngram, Vec::new()));
                entry.1.push(seq.chat_id);
            }
        }
    }

    let mut patterns: Vec<ToolPattern> = counts
        .into_iter()
        .filter(|(_, (_, chats))| chats.len() >= min_occurrences)
        .map(|(hash, (sequence, chats))| ToolPattern {
            occurrences: chats.len(),
            example_chat_ids: chats.into_iter().take(5).collect(),
            hash,
            sequence,
        })
        .collect();

    // Sort by occurrences descending, then by sequence length descending
    patterns.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then(b.sequence.len().cmp(&a.sequence.len()))
    });

    // Deduplicate: if a shorter pattern is a subsequence of a longer one with
    // equal or fewer occurrences, drop the shorter one.
    let mut filtered = Vec::new();
    for pat in &patterns {
        let dominated = filtered.iter().any(|existing: &ToolPattern| {
            existing.occurrences >= pat.occurrences
                && existing.sequence.len() > pat.sequence.len()
                && existing
                    .sequence
                    .windows(pat.sequence.len())
                    .any(|w| w == pat.sequence.as_slice())
        });
        if !dominated {
            filtered.push(pat.clone());
        }
    }

    filtered
}

// ---------------------------------------------------------------------------
// Skill generation via LLM
// ---------------------------------------------------------------------------

const SKILL_GEN_SYSTEM_PROMPT: &str = r#"You are a skill generator for RayClaw, an AI agent runtime.
Given a repeated tool-call pattern, generate a SKILL.md file that encapsulates this workflow as a reusable skill.

Output ONLY the SKILL.md content in this exact format:

---
name: <kebab-case-name>
description: <when to use this skill — describe the trigger, not just what it does>
trust_level: candidate
source: auto-generated
---

# <Skill Title>

<Instructions for using this skill, referencing the specific tools in the pattern>

Rules:
- name must be kebab-case, descriptive, under 30 characters
- description must explain WHEN to trigger, not just WHAT it does
- Do NOT reference bash, write_file, or edit_file tools — candidate skills are read-only
- Keep instructions concise (under 100 lines)"#;

pub async fn generate_skill_content(
    llm: &dyn LlmProvider,
    pattern: &ToolPattern,
) -> Result<String, String> {
    let user_msg = format!(
        "Generate a skill for this repeated tool pattern:\n\n\
         Tool sequence (appeared {} times across {} sessions):\n{}\n\n\
         Example chat IDs: {:?}",
        pattern.occurrences,
        pattern.example_chat_ids.len(),
        pattern.sequence.join(" -> "),
        pattern.example_chat_ids,
    );

    let response = llm
        .send_message(
            SKILL_GEN_SYSTEM_PROMPT,
            vec![Message {
                role: "user".into(),
                content: MessageContent::Text(user_msg),
            }],
            None,
        )
        .await
        .map_err(|e| format!("LLM call failed: {e}"))?;

    let text = response
        .content
        .iter()
        .filter_map(|b| {
            if let ResponseContentBlock::Text { text } = b {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("");

    if text.trim().is_empty() {
        return Err("LLM returned empty response".into());
    }

    Ok(text)
}

// ---------------------------------------------------------------------------
// Candidate skill validation + file operations
// ---------------------------------------------------------------------------

/// Forbidden tool references in candidate skills.
const FORBIDDEN_TOOLS: &[&str] = &["bash", "write_file", "edit_file"];

/// Validate generated skill content: must have valid frontmatter, no forbidden tools.
pub fn validate_candidate_content(content: &str) -> Result<(), String> {
    if !content.contains("---") {
        return Err("Missing YAML frontmatter".into());
    }
    let lower = content.to_lowercase();
    for tool in FORBIDDEN_TOOLS {
        // Check for tool references in the instructions (not in frontmatter)
        if let Some(body_start) = lower.rfind("---") {
            let body = &lower[body_start + 3..];
            if body.contains(tool) {
                return Err(format!("Candidate skill references forbidden tool: {tool}"));
            }
        }
    }
    Ok(())
}

/// Extract the skill name from generated SKILL.md content.
pub fn extract_skill_name(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name:") {
            let name = rest.trim().trim_matches('"').trim_matches('\'');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Write a candidate SKILL.md to skills/auto-generated/{name}/SKILL.md.
pub fn write_candidate_skill(
    skills_dir: &Path,
    name: &str,
    content: &str,
) -> Result<PathBuf, String> {
    let auto_dir = skills_dir.join("auto-generated").join(name);
    std::fs::create_dir_all(&auto_dir).map_err(|e| format!("mkdir failed: {e}"))?;
    let skill_path = auto_dir.join("SKILL.md");
    std::fs::write(&skill_path, content).map_err(|e| format!("write failed: {e}"))?;
    Ok(skill_path)
}

// ---------------------------------------------------------------------------
// Promotion: candidate -> verified
// ---------------------------------------------------------------------------

/// Check if a candidate skill should be promoted to verified.
/// Requires: 3+ activations AND success_rate > 0.7.
pub fn should_promote(health: &SkillHealthRow) -> bool {
    health.total_activations >= 3
        && health.successful_activations as f64 / health.total_activations as f64 > 0.7
}

/// Promote a skill by rewriting its trust_level in the SKILL.md frontmatter.
pub fn promote_skill(skills_dir: &Path, skill_name: &str) -> Result<(), String> {
    let skill_path = skills_dir
        .join("auto-generated")
        .join(skill_name)
        .join("SKILL.md");
    if !skill_path.exists() {
        return Err(format!("Skill file not found: {}", skill_path.display()));
    }
    let content = std::fs::read_to_string(&skill_path).map_err(|e| format!("read failed: {e}"))?;
    let updated = content.replace("trust_level: candidate", "trust_level: verified");
    std::fs::write(&skill_path, updated).map_err(|e| format!("write failed: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Archival (retirement)
// ---------------------------------------------------------------------------

/// Archive a skill to skills/.archive/{name}/ with a retirement metadata snapshot.
pub fn archive_skill(
    skills_dir: &Path,
    skill_name: &str,
    reason: &str,
    health: Option<&SkillHealthRow>,
) -> Result<(), String> {
    let archive_dir = skills_dir.join(".archive").join(skill_name);
    std::fs::create_dir_all(&archive_dir).map_err(|e| format!("mkdir failed: {e}"))?;

    let source_dir = find_skill_dir(skills_dir, skill_name)?;
    let src_skill = source_dir.join("SKILL.md");
    let dst_skill = archive_dir.join("SKILL.md");
    std::fs::copy(&src_skill, &dst_skill).map_err(|e| format!("copy failed: {e}"))?;

    // Write retirement metadata
    let meta = if let Some(h) = health {
        serde_json::json!({
            "archived_at": chrono::Utc::now().to_rfc3339(),
            "reason": reason,
            "metrics": {
                "total_activations": h.total_activations,
                "success_rate": h.successful_activations as f64 / h.total_activations.max(1) as f64,
                "avg_tokens": h.avg_tokens,
                "last_activated_at": h.last_activated_at,
            }
        })
    } else {
        serde_json::json!({
            "archived_at": chrono::Utc::now().to_rfc3339(),
            "reason": reason,
        })
    };
    let meta_path = archive_dir.join(".retirement_meta.json");
    std::fs::write(
        &meta_path,
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| format!("write meta failed: {e}"))?;

    // Remove original directory
    std::fs::remove_dir_all(&source_dir).map_err(|e| format!("remove failed: {e}"))?;
    Ok(())
}

fn find_skill_dir(skills_dir: &Path, skill_name: &str) -> Result<PathBuf, String> {
    let direct = skills_dir.join(skill_name);
    if direct.is_dir() {
        return Ok(direct);
    }
    let auto = skills_dir.join("auto-generated").join(skill_name);
    if auto.is_dir() {
        return Ok(auto);
    }
    Err(format!(
        "Skill directory not found for '{skill_name}' in {}",
        skills_dir.display()
    ))
}

// ---------------------------------------------------------------------------
// Learnings persistence (append-only JSONL)
// ---------------------------------------------------------------------------

/// Append a learning entry to data_dir/learnings.jsonl.
pub fn append_learning(data_dir: &str, source: &str, title: &str, context: &str, takeaway: &str) {
    let entry = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "source": source,
        "title": title,
        "context": context,
        "takeaway": takeaway,
        "confidence": 0.8,
    });
    let path = PathBuf::from(data_dir).join("learnings.jsonl");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", serde_json::to_string(&entry).unwrap_or_default());
        }
        Err(e) => {
            error!("Failed to write learning to {}: {e}", path.display());
        }
    }
}

// ---------------------------------------------------------------------------
// Rewrite failing skill via LLM
// ---------------------------------------------------------------------------

const SKILL_REWRITE_SYSTEM_PROMPT: &str = r#"You are a skill improvement specialist for RayClaw.
A skill has been failing frequently. Rewrite its SKILL.md to fix the issues.

Output ONLY the improved SKILL.md content, preserving the frontmatter format.
Keep the same name. Change trust_level to 'candidate' (it needs re-verification).
Do NOT reference bash, write_file, or edit_file tools."#;

pub async fn rewrite_failing_skill(
    llm: &dyn LlmProvider,
    current_content: &str,
    health: &SkillHealthRow,
) -> Result<String, String> {
    let user_msg = format!(
        "This skill is failing. Please rewrite it.\n\n\
         Success rate: {:.0}% ({} / {} activations)\n\
         Average tokens per use: {:.0}\n\n\
         Current SKILL.md:\n```\n{}\n```",
        health.successful_activations as f64 / health.total_activations.max(1) as f64 * 100.0,
        health.successful_activations,
        health.total_activations,
        health.avg_tokens,
        current_content,
    );

    let response = llm
        .send_message(
            SKILL_REWRITE_SYSTEM_PROMPT,
            vec![Message {
                role: "user".into(),
                content: MessageContent::Text(user_msg),
            }],
            None,
        )
        .await
        .map_err(|e| format!("LLM rewrite call failed: {e}"))?;

    let text = response
        .content
        .iter()
        .filter_map(|b| {
            if let ResponseContentBlock::Text { text } = b {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("");

    if text.trim().is_empty() {
        return Err("LLM returned empty rewrite".into());
    }

    Ok(text)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_ngrams() {
        let tools: Vec<String> = vec!["a", "b", "c", "d"]
            .into_iter()
            .map(String::from)
            .collect();
        let ngrams = extract_ngrams(&tools, 3);
        // Length 3: [a,b,c], [b,c,d] = 2
        // Length 4: [a,b,c,d] = 1
        assert_eq!(ngrams.len(), 3);
    }

    #[test]
    fn test_detect_patterns_basic() {
        let sequences = vec![
            ToolCallSequence {
                chat_id: 1,
                tool_names: vec!["web_search", "web_fetch", "todo_write"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            },
            ToolCallSequence {
                chat_id: 2,
                tool_names: vec!["web_search", "web_fetch", "todo_write"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            },
            ToolCallSequence {
                chat_id: 3,
                tool_names: vec!["web_search", "web_fetch", "todo_write"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            },
        ];

        let patterns = detect_patterns(&sequences, 3, 3);
        assert_eq!(patterns.len(), 1);
        assert_eq!(
            patterns[0].sequence,
            vec!["web_search", "web_fetch", "todo_write"]
        );
        assert_eq!(patterns[0].occurrences, 3);
    }

    #[test]
    fn test_detect_patterns_below_threshold() {
        let sequences = vec![
            ToolCallSequence {
                chat_id: 1,
                tool_names: vec!["a", "b", "c"].into_iter().map(String::from).collect(),
            },
            ToolCallSequence {
                chat_id: 2,
                tool_names: vec!["a", "b", "c"].into_iter().map(String::from).collect(),
            },
        ];
        // min_occurrences = 3, only 2 sessions
        let patterns = detect_patterns(&sequences, 3, 3);
        assert!(patterns.is_empty());
    }

    #[test]
    fn test_extract_skill_name() {
        let content = "---\nname: search-and-summarize\ndescription: test\n---\nbody";
        assert_eq!(
            extract_skill_name(content),
            Some("search-and-summarize".to_string())
        );
    }

    #[test]
    fn test_extract_skill_name_missing() {
        assert_eq!(extract_skill_name("no frontmatter here"), None);
    }

    #[test]
    fn test_validate_candidate_content_ok() {
        let content = "---\nname: test\ntrust_level: candidate\n---\nUse web_search to find info.";
        assert!(validate_candidate_content(content).is_ok());
    }

    #[test]
    fn test_validate_candidate_content_forbidden_tool() {
        let content = "---\nname: test\n---\nUse bash to run commands.";
        assert!(validate_candidate_content(content).is_err());
    }

    #[test]
    fn test_should_promote() {
        let health = SkillHealthRow {
            skill_name: "test".into(),
            total_activations: 5,
            successful_activations: 4,
            avg_tokens: 100.0,
            avg_duration_ms: 50.0,
            last_activated_at: "2026-04-22T00:00:00Z".into(),
            first_activated_at: "2026-04-20T00:00:00Z".into(),
        };
        assert!(should_promote(&health));

        let failing = SkillHealthRow {
            total_activations: 5,
            successful_activations: 1,
            ..health.clone()
        };
        assert!(!should_promote(&failing));

        let too_few = SkillHealthRow {
            total_activations: 2,
            successful_activations: 2,
            ..health
        };
        assert!(!should_promote(&too_few));
    }

    #[test]
    fn test_hash_sequence() {
        let seq: Vec<String> = vec!["a", "b", "c"].into_iter().map(String::from).collect();
        assert_eq!(hash_sequence(&seq), "a|b|c");
    }

    #[test]
    fn test_write_and_promote_candidate() {
        let dir =
            std::env::temp_dir().join(format!("rayclaw_skill_evo_test_{}", uuid::Uuid::new_v4()));
        let content = "---\nname: test-skill\ndescription: test\ntrust_level: candidate\nsource: auto-generated\n---\nInstructions here.";

        let path = write_candidate_skill(&dir, "test-skill", content).unwrap();
        assert!(path.exists());

        // Promote
        promote_skill(&dir, "test-skill").unwrap();
        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(updated.contains("trust_level: verified"));
        assert!(!updated.contains("trust_level: candidate"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_archive_skill() {
        let dir =
            std::env::temp_dir().join(format!("rayclaw_skill_evo_test_{}", uuid::Uuid::new_v4()));
        let skill_dir = dir.join("auto-generated").join("old-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: old-skill\n---\nold instructions",
        )
        .unwrap();

        let health = SkillHealthRow {
            skill_name: "old-skill".into(),
            total_activations: 10,
            successful_activations: 1,
            avg_tokens: 500.0,
            avg_duration_ms: 100.0,
            last_activated_at: "2026-03-01T00:00:00Z".into(),
            first_activated_at: "2026-02-01T00:00:00Z".into(),
        };

        archive_skill(&dir, "old-skill", "stale: 30+ days unused", Some(&health)).unwrap();

        // Original should be gone
        assert!(!skill_dir.exists());
        // Archive should exist
        let archive = dir.join(".archive").join("old-skill");
        assert!(archive.join("SKILL.md").exists());
        assert!(archive.join(".retirement_meta.json").exists());

        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(archive.join(".retirement_meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["reason"], "stale: 30+ days unused");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_append_learning() {
        let dir =
            std::env::temp_dir().join(format!("rayclaw_learning_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        append_learning(
            dir.to_str().unwrap(),
            "skill_retire",
            "Retired old-skill",
            "unused for 30 days",
            "auto-generated skills need regular use to stay active",
        );

        let content = std::fs::read_to_string(dir.join("learnings.jsonl")).unwrap();
        let entry: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(entry["source"], "skill_retire");
        assert_eq!(entry["title"], "Retired old-skill");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
