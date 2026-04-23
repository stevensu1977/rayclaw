# RayClaw Self-Evolution Roadmap

> 基于 Hermes Agent Skill 自进化、yoyo-evolve 源码自修改、OpenClaw Dreaming 三个机制的分析，
> 结合 RayClaw 现有架构（Rust/Tokio、scheduler/reflector、AGENTS.md 记忆、静态 skill 发现），
> 制定以 **Skill 进化 + 源码自修改** 为核心的自进化实施路线。
>
> 设计哲学：**Get job done, not daydream.** RayClaw 是工具执行型 agent，
> 进化的目标是扩展能力边界和提升执行精度，不是做记忆联想。
>
> 参考文档：`docs/SELF-EVOLUTION-PROPOSAL.md`（4 层温度模型）、`docs/rayclaw-evolution-tasks.md`（现有任务追踪）
>
> 创建日期：2026-04-21 | 修订：2026-04-21（重排优先级）

---

## 0. 现状分析

### RayClaw 已有的进化基础设施

| 组件 | 文件 | 能力 | 缺失 |
|------|------|------|------|
| **Reflector** | `scheduler.rs:spawn_reflector` | 定时执行（默认 15min）、嵌入回填、陈旧记忆归档 | 无 LLM 分析、无质量评估、无主动进化 |
| **Memory** | `memory.rs` | 全局/chat 两层 AGENTS.md、XML 注入 system prompt | 无评分、无衰减、无向量检索、无冲突检测 |
| **Memory Quality** | `memory_quality.rs` | 闲聊过滤、长度截断、显式 remember 指令提取 | 仅写入门控，无读取排序，无生命周期管理 |
| **Skills** | `skills.rs` | SKILL.md 发现、平台兼容性检查、YAML 元数据 | 静态只读、无使用追踪、无自动生成、无淘汰 |
| **Scheduler** | `scheduler.rs:spawn_scheduler` | cron 任务执行、agent loop 调用 | 仅执行预定义 prompt，不产生进化行为 |
| **Loop Detector** | `agent_engine.rs` | 环形缓冲区、hash 重复检测、3 轮触发 | 仅中断，不学习——检测到的模式不反馈到记忆 |
| **Error Classifier** | `error_classifier.rs` | 5 类分类、重试策略、overflow 恢复 | 不统计趋势，不做自适应调整 |

### 三个参考系统的核心启发

| 系统 | 核心机制 | RayClaw 借鉴优先级 |
|------|---------|-------------------|
| **Hermes Agent** | 多来源 skill registry（official/trusted/community）；模糊匹配；安装量+周活跃度追踪；信任分级 | **最高** — skill 生命周期管理是 RayClaw 最缺的闭环 |
| **yoyo-evolve** | Assessment→Plan→Impl 三阶段流水线；shell 层 protected-file guard；per-task revert；learnings.jsonl 持续积累 | **高** — 安全的源码自修改能力是终极进化形态 |
| **OpenClaw Dreaming** | Light/REM/Deep 三阶段；6 维评分模型；Jaccard 去重；dream narrative 生成 | **低** — 评分模型有参考价值，但 LLM dreaming 对工具型 agent 弊大于利 |

---

## 1. 分层架构

```
                    ┌───────────────────────────────────────────┐
                    │         RayClaw Self-Evolution Layers      │
                    │                                           │
     安全/自动 ◄────┼───────────────────────────────────────────► 激进/审批
                    │                                           │
  L0: Observation   │  ████████████  每次对话后 (被动收集)        │
  L1: Skills        │  ██████████    reflector 周期 (每日)       │
  L2: Source        │  ████████      PR 工作流 (人工审批)         │
  L3: Memory        │  ██████        reflector 周期 (统计规则)    │
  L4: Config        │  ████          评估周期 (每周)             │
                    │                                           │
                    │  ══════════════════════════════════════    │
                    │  冻结区: agent loop / config schema /       │
                    │         SOUL.md / CI workflows / 安全网     │
                    └───────────────────────────────────────────┘
```

核心路径：**Observation → Skill 进化 → 源码自修改**。记忆评分和 Config 调优是辅助。

---

## 2. Phase 0 — 可观测性基础（L0: Observation）

> **前提**：所有后续层的进化都依赖信号。没有信号源，进化就是盲目的。
> **工期**：1-2 周 | **风险**：极低 | **代码增量**：~400 行

### 2.0.1 会话指标收集器

在 agent loop 的 `process_with_agent` 退出时，写入结构化指标：

```rust
struct SessionMetrics {
    chat_id: i64,
    channel: String,
    timestamp: String,           // ISO 8601
    total_iterations: u32,
    tool_calls: Vec<ToolCallMetric>,  // name, duration_ms, success
    llm_input_tokens: u64,
    llm_output_tokens: u64,
    error_count: u32,
    error_categories: Vec<String>,    // from ErrorClassifier
    loop_detected: bool,
    overflow_recovered: bool,
    user_corrections: u32,       // "不对"/"错了" 关键词检测
    session_duration_ms: u64,
}
```

**存储**：SQLite 新表 `session_metrics`，由 reflector 定期聚合。

**切入点**：`agent_engine.rs` — `process_with_agent` 返回前收集；`db.rs` — 新表 + 写入方法。

### 2.0.2 工具使用追踪

每次 tool 调用记录 `(tool_name, params_hash, success, duration_ms, timestamp)`。

这是 Skill 使用率追踪（Phase 1）和 Tool Dedup（已在 evolution-tasks.md 1.4）的前置。

**切入点**：`agent_engine.rs` tool execution 处。

### 2.0.3 用户反馈信号检测

简单关键词检测（先不用 LLM）：

| 信号 | 关键词 | 权重 |
|------|--------|------|
| 纠正 | "不对","错了","wrong","no that's not" | 高 |
| 重复 | 同一用户在 24h 内问相同问题（精确匹配或高相似度） | 高 |
| 正向 | "谢谢","perfect","exactly" | 低（baseline） |
| 工具循环 | loop detector 触发 | 高 |

**切入点**：`agent_engine.rs` 用户消息预处理阶段。

---

## 3. Phase 1 — Skill 进化（L1: Skills）

> **目标**：从静态 skill 发现进化到自动生成/淘汰/优化的 skill 生态。
> **工期**：3-4 周 | **风险**：中低 | **代码增量**：~800 行
> **依赖**：Phase 0 的 tool 使用追踪
> **核心参考**：Hermes Agent 的 skill registry + 使用追踪 + 信任分级

### 3.1.1 Skill 使用率追踪

每次 skill 激活写入 `.metrics.jsonl`：

```json
{"skill": "code-review", "ts": "2026-05-01T10:00Z", "success": true, "tokens": 1200, "duration_ms": 3400}
```

Reflector 周期聚合为 `SkillHealthReport`：

| 指标 | 淘汰阈值 | 预警阈值 |
|------|---------|---------|
| days_since_last_use | > 30d | > 14d |
| success_rate | < 0.2 | < 0.5 |
| avg_tokens_per_use | > 10,000 | > 5,000 |

**切入点**：`skills.rs` 扩展 `SkillMetadata` + `agent_engine.rs` skill 激活处。

### 3.1.2 Skill 自动生成

检测到以下信号时，自动生成 skill：

| 信号 | 检测方法 | 触发条件 |
|------|---------|---------|
| 重复工具组合 | tool_calls 序列模式匹配 | 同一 3+ tool 序列出现 3+ 次 |
| 重复用户请求 | 用户消息精确/模糊匹配聚类 | 同类请求 5+ 次/周 |
| 用户显式请求 | "记住怎么做这个"关键词 | 单次触发 |

**生成流程**：
1. Reflector 识别模式 → 调用 LLM 产出 skill 草案（SKILL.md YAML frontmatter + instructions）
2. 写入 `skills/auto-generated/` 目录
3. 标记 `source: "auto-generated"`, `trust_level: "candidate"`, `confidence: 0.7`
4. 首次使用后根据成功率决定是否晋升为正式 skill（`trust_level: "verified"`）

**信任分级**（借鉴 Hermes）：

| 级别 | 来源 | 权限 |
|------|------|------|
| `official` | 手动编写，随代码发布 | 所有工具 |
| `verified` | 自动生成，经验证 | 只读工具 + bash（受限） |
| `candidate` | 自动生成，未验证 | 仅只读工具 |
| `archived` | 淘汰 | 不加载 |

**安全网**：`candidate` 级 skill 不含 `bash`、`write_file`、`edit_file` 工具引用。晋升到 `verified` 需要：使用 3+ 次 且 success_rate > 0.7。

### 3.1.3 Skill 淘汰管线

```
stale (>30d unused) → archive/ + log learning
failing (<20% success for 2 weeks) → LLM rewrite attempt (1 次) → archive if still failing
```

归档路径：`skills/.archive/{skill_name}/`，保留 SKILL.md + metrics 快照 + 淘汰原因。

淘汰事件写入 `learnings.jsonl`，供后续 Phase 2 源码进化参考。

---

## 4. Phase 2 — 源码自修改（L2: Source）

> **目标**：RayClaw 能识别自身代码的改进点，并以 PR 方式提交。
> **工期**：4-6 周 | **风险**：高 | **代码增量**：~1,500 行
> **依赖**：Phase 0 指标 + Phase 1 learnings 积累
> **强制要求**：所有源码变更必须人工审批 merge
> **核心参考**：yoyo-evolve 的 Assessment→Plan→Impl 流水线 + shell guard

### 4.2.1 Evolution Pipeline

```
每周触发（或手动 `/evolve`）：

Assessment Agent
  ├─ 输入: learnings.jsonl (近 30 天)
  │        session_metrics 聚合
  │        skill health reports
  │        cargo test 结果
  │        cargo clippy 输出
  └─ 输出: assessment.md (问题列表 + 优先级)

Planning Agent
  ├─ 输入: assessment.md
  │        当前代码结构 (tree + 关键文件摘要)
  └─ 输出: tasks.json (最多 3 个任务, 每任务最多改 3 文件)

Implementation Agent (per task, on branch)
  ├─ 输入: task description
  │        relevant source files
  ├─ 执行: 编辑代码 → cargo test → cargo clippy
  ├─ 安全网: protected file guard (shell 层)
  └─ 输出: git commit on evolve/ branch

PR Creation
  └─ gh pr create --label "self-evolve" --label "needs-review"
```

### 4.2.2 Learnings 持久化

Evolution pipeline 的关键输入来自日常运行积累的 learnings：

```rust
struct Learning {
    timestamp: String,
    source: String,      // "error" | "user_feedback" | "loop_detection" | "skill_retire" | "tool_failure"
    title: String,
    context: String,
    takeaway: String,
    confidence: f64,
}
```

存储：`data_dir/learnings.jsonl`（追加写）+ `data_dir/active_learnings.md`（reflector 周期合成）。

Active learnings 注入 system prompt（与 AGENTS.md 并列），影响后续对话行为，同时作为 Assessment Agent 的输入。

**信号来源**（确定性信号优先，不依赖 LLM 推断）：

| 来源 | 触发条件 | 写入内容 |
|------|---------|---------|
| Error Classifier | 同一 error category 在 7 天内出现 5+ 次 | 错误模式 + 上下文 |
| Loop Detector | 检测到循环 | 导致循环的 tool 序列 + 触发条件 |
| User Feedback | "不对"/"错了" 关键词 | 用户消息 + agent 上一轮回答摘要 |
| Skill 淘汰 | Phase 1 淘汰管线触发 | skill 名称 + 淘汰原因 + 使用统计 |
| Tool 失败 | 同一 tool 在 7 天内失败 5+ 次 | tool 名称 + 常见错误 |

### 4.2.3 安全区域划分

```
冻结区（永不可改）:
  src/main.rs              # 入口
  src/agent_engine.rs      # 核心循环 (agent 不能改自己的 loop)
  src/config.rs            # 配置 schema
  src/db.rs                # 数据库 schema
  .github/                 # CI 流水线
  scripts/                 # 运维脚本
  SOUL.md                  # 身份定义
  Cargo.toml               # 依赖管理（版本号除外）

安全区（可改，低风险）:
  src/tools/*.rs           # 新增/改进工具
  src/skills.rs            # skill 系统增强
  docs/                    # 文档
  tests/                   # 测试（只增不删）

受控区（可改，中风险，需额外测试覆盖）:
  src/channels/*.rs        # 渠道适配器
  src/memory*.rs           # 记忆系统
  src/scheduler.rs         # 调度器
  src/llm*.rs              # LLM 调用层
```

### 4.2.4 外部安全网（Shell Guard）

```bash
#!/bin/bash
# scripts/evolve_guard.sh — 在 agent 外部运行，不可被 agent 修改

FROZEN_PATHS=(
    "src/main.rs"
    "src/agent_engine.rs"
    "src/config.rs"
    "src/db.rs"
    ".github/"
    "scripts/"
    "SOUL.md"
    "Cargo.toml"
)

check_frozen() {
    local pre_sha=$1
    for pattern in "${FROZEN_PATHS[@]}"; do
        if git diff --name-only "$pre_sha"..HEAD | grep -q "^$pattern"; then
            echo "FROZEN FILE VIOLATION: $pattern"
            git reset --hard "$pre_sha"
            return 1
        fi
    done
}

# cargo test 必须通过
verify_tests() {
    cargo test 2>&1 || { echo "TEST FAILURE — reverting"; git reset --hard "$1"; return 1; }
    cargo clippy --all-targets -- -D warnings 2>&1 || { echo "CLIPPY FAILURE — reverting"; git reset --hard "$1"; return 1; }
}
```

### 4.2.5 对话回放测试

```
Golden Conversations (data_dir/golden_tests/):
  - 手动挑选 10-20 个高质量对话
  - 每个包含: input messages + expected behavior (不是精确输出)
  - Evolution PR 必须通过回放测试: LLM judge 评分 ≥ 原始评分

评估维度:
  - 相关性: 回答是否切题
  - 工具使用: 是否正确选择工具
  - 安全性: 是否泄露信息或执行危险操作
```

---

## 5. Phase 3 — 记忆评分（L3: Memory）

> **目标**：从"写了就忘不了"进化到"记住该记的，忘掉该忘的"。
> **工期**：2 周 | **风险**：低 | **代码增量**：~500 行
> **依赖**：Phase 0 的指标收集
> **注意**：仅做统计评分，不做 LLM dreaming

### 5.3.1 记忆评分模型（纯统计，无 LLM）

```
confidence = frequency × 0.35 + recency × 0.30 + user_signal × 0.25 + quality × 0.10

frequency:   被引用/提及的次数（从 session_metrics 统计）
recency:     距上次引用的天数衰减 = 2^(-age_days / half_life)
user_signal: 用户显式确认(+1.0) / 纠正(-0.5) / 无反馈(0)
quality:     memory_quality.rs 现有规则转换为 0-1 分数
```

**半衰期配置**：
- Core（用户显式 remember）：∞（不衰减）
- Normal：14 天
- Ephemeral（自动提取）：3 天

**切入点**：`memory.rs` 扩展，`db.rs` memories 表增加 `importance`, `score`, `last_referenced_at` 字段。

### 5.3.2 Reflector 增强：统计淘汰

将现有 reflector 增强为统计驱动的记忆管理：

```
reflector 周期（每 6h 或 reflector_interval_mins 配置）:
  1. 收集上一周期新增的 session_metrics
  2. 统计 memory 引用频率，更新 scores
  3. 低分记忆（score < 0.2 且 age > 30d）→ 标记 stale → 后续归档
  4. 高分记忆（score > 0.8 且在 chat 级别）→ 候选晋升到 global
```

不使用 LLM，纯统计规则，零额外 API 成本。

**切入点**：`scheduler.rs:run_reflector` 扩展逻辑。

### 5.3.3 记忆冲突检测（可选，轻量 LLM）

唯一允许 LLM 介入记忆系统的场景——不是做梦，而是做**事实校验**：

当 reflector 发现两条记忆语义相似（Jaccard > 0.7 或 embedding 距离 < 0.2）时，
调用 LLM 判断："这两条记忆是否矛盾？"

- 输出是布尔判断 + 保留哪条，不是创造性内容
- 频率极低（仅在发现疑似冲突时触发）
- 可选，不影响核心流程

---

## 6. Phase 4 — Config 自调优（L4: Config）

> **目标**：让 RayClaw 根据使用模式自动调整运行时参数。
> **工期**：2-3 周 | **风险**：中 | **代码增量**：~600 行
> **依赖**：Phase 0 的指标收集

### 6.4.1 可调参数白名单

```yaml
evolvable_config:
  allowed:
    - max_tool_iterations        # 根据任务复杂度
    - max_session_messages       # 根据对话长度趋势
    - compact_keep_recent        # 根据 overflow 频率
    - reflector_interval_mins    # 根据活跃度
    - memory_token_budget        # 根据记忆量
    - llm_idle_timeout_secs      # 根据响应延迟统计
  forbidden:
    - api_key / bot_token        # 凭证永不可改
    - llm_provider / model       # 模型选择需人工
    - channels.*                 # 通道配置需人工
    - working_dir / data_dir     # 路径不可改
    - web_auth_token             # 安全不可改
```

### 6.4.2 Config Tuner（规则引擎，不用 LLM）

每周运行一次（scheduler cron），输入 = 一周的 session_metrics 聚合：

```
IF avg_iterations > 0.8 × max_tool_iterations THEN
  propose(max_tool_iterations, current + 5, confidence=0.8)

IF overflow_events > 3/week THEN
  propose(compact_keep_recent, current - 5, confidence=0.7)

IF p95_llm_latency > llm_idle_timeout_secs × 0.8 THEN
  propose(llm_idle_timeout_secs, current × 1.5, confidence=0.6)
```

### 6.4.3 安全网

**变更约束**：
- 单次调整幅度 ≤ ±30%
- 每周最多调整 2 个参数
- 硬性边界：`max_tool_iterations ∈ [5, 50]`，`llm_idle_timeout_secs ∈ [10, 300]`

**回滚机制**：
- 每次变更记录 `config_history.jsonl`（before, after, reason, timestamp）
- 24h 观察期：如果变更后 error_rate 上升 >50% → 自动 revert
- Revert 事件写入 learnings.jsonl

**切入点**：新的 `src/config_tuner.rs`，由 scheduler 触发。Config 变更写入 `config_overrides.json`，启动时与 YAML 合并。

---

## 7. Deferred — LLM Dreaming（不推荐优先实施）

> **状态**：延后，待 Phase 0-2 完成且 learnings 数据充分后再评估
> **原因**：RayClaw 是工具执行型 agent，记忆精确度优先于创造性联想

如果未来决定实施，约束条件：

- **仅做 dedup/merge**：LLM 判断两条记忆是否重复，决定保留哪条。不做 narrative synthesis。
- **两阶段提交**：Dream 产出 `dream_report.json`，由 reflector 下一周期执行。不直接修改 AGENTS.md。
- **不做 conceptual association**：不让 LLM 在不相关记忆之间建立联系。对工具型 agent，假阳性关联 = 错误的 tool call。
- **频率低**：最多每周一次，不是每日。

OpenClaw 评分模型中可借鉴的部分（frequency×0.45 + recall×0.25 + consolidation×0.2 + conceptual×0.1）
已在 Phase 3 的纯统计评分中吸收。Jaccard 去重（0.9 阈值）已纳入 Phase 3.3 冲突检测。

---

## 8. 实施时间线

```
Month 1 (Week 1-4):
  Phase 0: Observation        [Week 1-2]  ← 基础设施，所有后续依赖此
  Phase 1.1: Skill Tracking   [Week 2-3]  ← 使用率追踪
  Phase 1.2: Skill Auto-Gen   [Week 3-4]  ← 自动生成 + 信任分级

Month 2 (Week 5-8):
  Phase 1.3: Skill Retire     [Week 5]    ← 淘汰管线，闭环完成
  Phase 2.1: Learnings        [Week 5-6]  ← learnings.jsonl 积累
  Phase 2.2: Evolution Pipeline[Week 6-8] ← Assessment→Plan→Impl

Month 3 (Week 9-12):
  Phase 2.3: Safety Net       [Week 9-10] ← Shell guard + golden tests
  Phase 3: Memory Scoring     [Week 10-11]← 统计评分 + reflector 增强
  Phase 4: Config Tuning      [Week 11-12]← 白名单 + tuner + rollback

Deferred:
  LLM Dreaming                            ← 待评估，非必须
```

### MVP（最小闭环）

**Phase 0 + Phase 1.1 + Phase 1.2**（~4 周）：

```
tool_calls 追踪
  → reflector 聚合 skill 使用率
  → 检测重复 tool 序列模式
  → 自动生成 candidate skill
  → 使用追踪验证 candidate 效果
  → 晋升或淘汰
```

这是一个"观测 → 识别模式 → 生成 skill → 验证 → 反馈"的完整闭环。
产出是**可执行的新能力**，不是记忆整理。每一轮进化都直接扩展 agent 的能力边界。

---

## 9. 关键设计决策

### 9.1 为什么 Skill 进化优先于记忆进化？

记忆进化（评分、淘汰）改善的是**信息检索质量**——agent 能更好地找到已知信息。
Skill 进化改善的是**能力边界**——agent 能做之前做不到的事。

对用户来说，"agent 学会了一个新技能"比"agent 清理了旧记忆"有直接可感知的价值。

### 9.2 为什么源码自修改提前到 Phase 2？

yoyo-evolve 证明了 Rust 项目的自修改是可行的——`cargo test` + `cargo clippy` 提供了强类型安全网。
关键前置是 **learnings 积累**（Phase 2.1），而非记忆系统完善。

只要有：
1. session_metrics（Phase 0）提供问题信号
2. learnings.jsonl（Phase 2.1）提供改进方向
3. shell guard（Phase 2.3）提供安全网

就可以开始源码进化。不需要等记忆评分或 config 调优完成。

### 9.3 为什么 LLM Dreaming 被降级？

RayClaw 是工具执行型 agent，不是社交对话型 agent。两者对记忆的需求根本不同：

| | 社交型 (OpenClaw) | 工具型 (RayClaw) |
|---|---|---|
| 记忆目标 | 叙事连贯性 | 任务精确性 |
| 有价值的联想 | "用户喜欢徒步" + "天气预报" → 主动推荐 | 几乎不存在 |
| 错误联想的代价 | 尴尬的闲聊 | 错误的文件编辑/配置操作 |
| LLM Dreaming ROI | 高 | 低（精度风险 > 洞察收益） |

把 LLM 预算花在 skill 生成和源码进化上，ROI 远高于记忆做梦。

### 9.4 冻结区为什么包含 agent_engine.rs？

Agent 不能修改自己的思考循环。这是所有自修改系统的核心安全原则：
- yoyo-evolve 保护 `.yoyo.toml` 和核心 loop
- OpenClaw 的 dreaming 不能修改 dreaming controller 自身
- 如果 agent 能修改 agent_engine.rs，它就能绕过所有安全网

---

## 10. 与现有 evolution-tasks.md 的关系

| evolution-tasks.md | 本 roadmap | 关系 |
|-------------------|-----------|------|
| Phase 1: Resilience (error/overflow/loop) | **已完成**，是本 roadmap 的前置 | 前置条件 |
| Phase 2: Memory (consolidation/decay/vector) | Phase 3 (Memory Scoring) | 合并——仅保留统计评分，砍掉 LLM dreaming |
| Phase 3: Intelligence (token/compaction/failover) | 独立进行 | 并行——可以和本 roadmap 的 Phase 0-1 同步推进 |
| 无 | Phase 0 (Observation) | **新增** |
| 无 | Phase 1 (Skill Evolution) | **新增** — 最高优先级 |
| 无 | Phase 2 (Source Evolution) | **新增** — 第二优先级 |
| 无 | Phase 4 (Config Tuning) | **新增** |

---

## 11. 度量标准

### 进化健康度仪表盘（Phase 0 完成后可实现）

| 指标 | 计算方式 | 健康阈值 |
|------|---------|---------|
| **Skill Utilization** | % of skills used in last 30d | > 50% |
| **Skill Success Rate** | successful activations / total activations | > 70% |
| **Skill Generation Rate** | new skills created / month | 观察期 |
| **Evolution PR Merge Rate** | merged PRs / total evolution PRs | 观察期 |
| **Conversation Efficiency** | avg iterations per response | < 5 |
| **Error Recovery Rate** | recovered errors / total errors | > 80% |
| **User Correction Rate** | corrections / total responses | < 10% |
| **Loop Frequency** | loop detections / total sessions | < 5% |
| **Memory Freshness** | % of memories referenced in last 14d | > 60% |
| **Config Stability** | % of weeks without config revert | > 90% |

---

*本文档是方案，不修改代码。实施从 Phase 0 开始，核心路径为 Skill 进化 → 源码自修改。*
