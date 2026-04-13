# RayClaw Agent Core Evolution — Task Tracker

> Source: [rayclaw-evolution-roadmap.md](/home/ubuntu/rayclaw-evolution-roadmap.md)
> Created: 2026-03-31
> Target: 42K → 48K lines (+14%), 12 weeks

---

## Legend

- `[ ]` Not started
- `[~]` In progress
- `[x]` Completed
- `[-]` Deferred / Dropped

---

## Phase 1: Resilience Foundation (3-4 weeks, ~1,500 lines)

> Goal: from "it runs" to "it doesn't crash"

### 1.1 Error Classifier

> Classify LLM errors into transient / permanent / rate-limit and route to retry / abort / fallback accordingly.
> Reference: OpenClaw 5-layer recovery

- [x] Define error category enum: `Transient`, `Permanent`, `RateLimit`, `ContextOverflow`, `Auth`
- [x] Implement classifier for Anthropic native API error responses
- [x] Implement classifier for OpenAI-compatible API error responses
- [x] Implement classifier for Bedrock API error responses
- [x] Add retry policy per category (exponential backoff for transient, immediate abort for permanent)
- [x] Add network error classification and retry (connection refused, timeout, DNS)
- [x] Unit tests for each error category (18 tests)
- [x] Add rate-limit handler with `Retry-After` header respect
- [x] Integration test: simulate 429 → backoff → retry → success (9 tests)

### 1.2 Context Overflow Recovery

> Detect token overflow → auto-truncate old messages → retry.
> Reference: OpenClaw 3-layer overflow strategy

- [x] Detect overflow error from LLM response via `ContextOverflow` error variant + error classifier
- [x] Layer 1: Aggressive compaction — summarize all but last 4 messages via `compact_messages()`
- [x] Layer 2: Emergency truncation — keep only last 2 messages + user-role guard
- [x] On all layers fail: return user-friendly message suggesting `/reset`
- [x] `ClassifiedError::into_error()` routes ContextOverflow to dedicated error variant
- [x] Log which recovery layer was triggered
- [x] Tests: 8 tests (layer 1 compaction, layer 2 truncation, all-fail message, too-few-messages, error variant, into_error conversion)

### 1.3 LLM Idle Timeout

> Detect stalled streaming (no new tokens) → abort → retry or error.
> Reference: OpenClaw #55072

- [x] Add `llm_idle_timeout_secs` config option (default: 30s, minimum 5s)
- [x] Implement streaming watchdog: `tokio::time::timeout()` wrapping `byte_stream.next().await` in all 3 providers
- [x] On timeout: abort request, return error to user with explanation
- [x] Applied to Anthropic, OpenAI-compatible, and Bedrock streaming loops
- [x] Tests: 5 unit tests (default value, custom value, minimum enforced, OpenAI provider, YAML parsing)

### 1.4 Tool Call Deduplication

> Merge consecutive identical tool calls (same name + same params) to avoid redundant execution.

- [ ] Track last N tool calls in agent loop (name + params hash)
- [ ] Detect duplicate within sliding window (default: 3)
- [ ] Return cached result for duplicate call
- [ ] Log deduplication events
- [ ] Test: repeated `read_file` with same path returns cached result

### 1.5 Loop Detector

> Detect repetitive tool call patterns → force exit + notify user.
> Reference: ZeroClaw Loop Detector

- [x] Track tool call sequence as ring buffer in agent loop
- [x] Detect exact repetition: same [tool_name, params_hash] pattern repeating 3+ times
- [ ] Detect semantic repetition: same tool_name with minor param variations 5+ times
- [x] On detection: inject system message "Loop detected, stopping" → force `end_turn`
- [x] Add `max_loop_repeats` config option (default: 3)
- [x] Test: simulate loop scenario → detector triggers after configured threshold (8 tests)

### Phase 1 Milestones

- [x] Error classifier covers all LLM providers (Anthropic + OpenAI-compatible + Bedrock)
- [x] Overflow recovery E2E test passes (simulated overflow → Layer 1/2 recovery)
- [x] Loop detector triggers after 3 repetitions
- [ ] All new code < 1,500 lines total

---

## Phase 2: Memory Evolution (4-5 weeks, ~3,000 lines)

> Goal: from "remembers" to "remembers well"

### 2.1 LLM Consolidation

> Periodically use LLM to merge / deduplicate / summarize old memories.
> Reference: ZeroClaw Consolidation

- [ ] Design consolidation prompt: input N memories → output merged set
- [ ] Implement consolidation scheduler (run daily or on memory count threshold)
- [ ] Preserve original memories as archive, consolidation generates index summaries
- [ ] Add `consolidation_threshold` config (default: trigger at 100 memories)
- [ ] Add `consolidation_ratio` target (100 → 30 or fewer)
- [ ] Dry-run mode: show proposed merges without executing
- [ ] Test: 100 memories → consolidation produces ≤ 30

### 2.2 Time Decay

> `score × 2^(-age_days/7)`, Core category never decays.
> Reference: ZeroClaw decay formula

- [ ] Add `importance` field to memory records (core / normal / ephemeral)
- [ ] Implement decay function: `score × 2^(-age_days / half_life)`
- [ ] Core memories: half_life = ∞ (no decay)
- [ ] Normal memories: half_life = 7 days (default)
- [ ] Ephemeral memories: half_life = 1 day
- [ ] Apply decay at retrieval time (not storage time)
- [ ] Migration: add `importance` column to existing memories table
- [ ] Test: verify decay math and Core exemption

### 2.3 Embedding Vector Search

> sqlite-vec integration, generate embeddings on memory write.
> Reference: ZeroClaw Embedding

- [ ] Evaluate sqlite-vec vs embedded HNSW vs external Qdrant
- [ ] Integrate chosen solution (prefer sqlite-vec for single-binary)
- [ ] Generate embeddings on memory write (provider: configurable, default OpenAI ada-002)
- [ ] Add `embedding_provider` and `embedding_model` config options
- [ ] Store embeddings in dedicated table linked to memories
- [ ] Implement cosine similarity search
- [ ] Fallback: if embedding fails, gracefully degrade to keyword search
- [ ] Test: recall@10 > 0.8 compared to pure keyword search

### 2.4 Retrieval Pipeline

> Hybrid retrieval: hybrid 0.7 + importance 0.2 + recency 0.1
> Reference: ZeroClaw RRF

- [ ] Implement keyword search (existing FTS or LIKE)
- [ ] Implement vector search (from 2.3)
- [ ] Reciprocal Rank Fusion (RRF) to combine keyword + vector results
- [ ] Final scoring: `hybrid_score * 0.7 + importance * 0.2 + recency * 0.1`
- [ ] Configurable weights via config
- [ ] Test: hybrid retrieval outperforms keyword-only on test dataset

### 2.5 Namespace Isolation

> Isolate memories by chat / user / global to prevent cross-contamination.
> Reference: ZeroClaw namespace

- [ ] Add `namespace` field to memory records: `global`, `user:{id}`, `chat:{channel}:{id}`
- [ ] Write operations: auto-assign namespace based on caller context
- [ ] Read operations: search within caller's namespace + global
- [ ] Migration: backfill existing memories to `global` namespace
- [ ] Test: 100% no cross-contamination between namespaces

### 2.6 Conflict Detection

> Flag or overwrite when new memory contradicts old memory.
> Reference: ZeroClaw Conflict Detection

- [ ] On memory write: search for semantically similar existing memories
- [ ] Detect contradiction (LLM-based or embedding distance heuristic)
- [ ] Strategy: newer memory wins, old memory marked as `superseded`
- [ ] Log conflicts for observability
- [ ] Test: contradicting memory correctly supersedes old one

### Phase 2 Milestones

- [ ] Consolidation compresses 100 memories to ≤ 30
- [ ] Vector search recall@10 > 0.8 (vs keyword-only)
- [ ] Namespace isolation: 100% no cross-contamination
- [ ] All new code < 3,000 lines total

---

## Phase 3: Intelligence Hub (3-4 weeks, ~2,000 lines)

> Goal: from "usable" to "great"

### 3.1 Accurate Token Estimation

> Replace rough character count with proper estimation, CJK-aware.
> Reference: OpenClaw CJK FTS5 fix

- [ ] Implement tiktoken-compatible estimator (or use `tiktoken-rs` crate)
- [ ] Handle CJK characters correctly (1 CJK char ≈ 2-3 tokens)
- [ ] Replace all `content.len() / 4` heuristics in agent_engine.rs
- [ ] Add `estimate_tokens(text) -> usize` utility function
- [ ] Test: CJK token estimation error < 5%

### 3.2 Proactive Compaction

> Compress history messages when context approaches limit, before overflow.
> Reference: OpenClaw context pruner

- [ ] Track running token count across session messages
- [ ] Trigger compaction at 80% of model's context window (configurable)
- [ ] Smarter compaction: preserve tool_use/tool_result pairs, summarize conversation turns
- [ ] Integrate with token estimator (3.1)
- [ ] Test: session stays under context limit across 50+ turns

### 3.3 Model Failover

> Auto-switch to fallback model when primary provider is unavailable.
> Reference: OpenClaw Auth Profile rotation

- [ ] Add `fallback_models` config: ordered list of (provider, model) pairs
- [ ] On provider error (5xx, timeout, auth failure): try next in list
- [ ] Maintain per-provider health status (circuit breaker pattern)
- [ ] Recover: periodically retry primary after cooldown
- [ ] Log failover events with provider + error details
- [ ] Test: primary down → automatic switch to fallback → response succeeds

### 3.4 Hook System

> Before/after lifecycle hooks for tool calls, messages, and errors.
> Reference: MicroClaw hooks

- [ ] Define hook points: `before_tool`, `after_tool`, `before_llm`, `after_llm`, `on_error`
- [ ] Hook interface: async function receiving event context, returning allow/deny/modify
- [ ] Built-in hooks: logging, metrics, rate limiting
- [ ] User-configurable hooks via config (shell command or webhook URL)
- [ ] Test: hook fires on tool call, can block execution

### 3.5 Agent Observability

> Key metrics instrumentation: loop count, token usage, tool latency, error rate.
> Reference: MicroClaw OTLP metrics

- [ ] Define metrics: `agent_loop_iterations`, `llm_tokens_total`, `tool_duration_ms`, `error_count`
- [ ] In-process metrics collector (no external dependency required)
- [ ] Expose via `/api/metrics` endpoint (Prometheus format)
- [ ] Optional: OTLP export for external observability stack
- [ ] Dashboard integration: surface key metrics in Web UI
- [ ] Test: metrics increment correctly during agent loop execution

### Phase 3 Milestones

- [ ] CJK token estimation error < 5%
- [ ] Failover switch latency < 2s
- [ ] Hook system supports ≥ 5 lifecycle events
- [ ] All new code < 2,000 lines total

---

## Ecosystem Layer (post-core stabilization)

### Eco Phase 1: China Market Channels (4 weeks)

- [ ] WeCom (企业微信) channel adapter — text / image / file
- [ ] DingTalk (钉钉) channel adapter — robot API
- [ ] Feishu write upgrade — rich text, file upload, merge-forward
- [ ] Alibaba Cloud Bailian provider — Qwen model series

### Eco Phase 2: SDK & Distribution (6 weeks)

- [ ] SDK-as-Library — embeddable in Tauri / Next.js apps
- [ ] Feishu App Store listing
- [ ] DingTalk App Store listing
- [ ] China cloud one-click deploy templates (Alibaba Cloud / Tencent Cloud)

### Eco Phase 3: Enterprise Capabilities (8 weeks)

- [ ] Office automation tool suite — calendar / approval / tasks / sheets integration
- [ ] Knowledge base RAG — BM25 + sqlite-vec hybrid retrieval
- [ ] Multi-agent enterprise orchestration — supervisor → worker pattern

---

## Risk Register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| sqlite-vec ecosystem immaturity | Medium | High | Fallback: embedded HNSW or external Qdrant |
| LLM Consolidation quality instability | High | Medium | Keep originals, consolidation only generates index summaries |
| Code bloat exceeding 48K target | Medium | Low | Weekly code audit, over-budget features become optional plugins |
| Embedding API cost / latency | Medium | Medium | Batch writes, cache embeddings, allow local models |

---

## Progress Summary

| Phase | Status | Lines Added | Target |
|-------|--------|-------------|--------|
| Phase 1: Resilience | `[~]` In progress (1.1, 1.2, 1.3, 1.5 done) | ~550 | ~1,500 |
| Phase 2: Memory | `[ ]` Not started | 0 | ~3,000 |
| Phase 3: Intelligence | `[ ]` Not started | 0 | ~2,000 |
| Eco Phase 1 | `[ ]` Not started | — | — |
| Eco Phase 2 | `[ ]` Not started | — | — |
| Eco Phase 3 | `[ ]` Not started | — | — |
| **Total** | | **~550** | **~6,500** |

---

*Last updated: 2026-04-05*
