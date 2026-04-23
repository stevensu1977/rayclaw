# Self-Evolution: Completed Phases

> Phase 0 (Observation) 和 Phase 1 (Skill Evolution) 的实现记录。
> 实施日期：2026-04-21 ~ 2026-04-22
> 对应 roadmap：`docs/SELF-EVOLUTION-ROADMAP.md`

---

## Phase 0 — 可观测性基础 (Observation)

**状态**：已完成 ✓  
**代码增量**：~500 行  
**新增文件**：`src/metrics.rs`  
**修改文件**：`src/agent_engine.rs`, `src/db.rs`, `src/scheduler.rs`, `src/lib.rs`

### 实现内容

#### 1. 会话指标收集器 (`src/metrics.rs`)

每次 `process_with_agent` 调用自动收集：

```rust
SessionMetrics {
    chat_id, channel, timestamp,
    total_iterations,              // agent loop 迭代次数
    tool_calls: Vec<ToolCallMetric>, // 每次工具调用的 name/duration/success/error
    llm_input_tokens, llm_output_tokens,
    error_count, error_categories,
    loop_detected, overflow_recovered,
    user_corrections,              // 关键词检测：纠正/正面反馈
    session_duration_ms,
}
```

#### 2. DB Schema v4→v5

新增两张表：

| 表 | 字段 | 用途 |
|---|---|---|
| `session_metrics` | chat_id, channel, timestamp, total_iterations, tool_call_count, llm_input/output_tokens, error_count, error_categories, loop_detected, overflow_recovered, user_corrections, session_duration_ms | 会话级聚合指标 |
| `tool_call_logs` | chat_id, tool_name, success, duration_ms, error_type, timestamp, session_metric_id | 单次工具调用明细 |

索引：`tool_call_logs(chat_id, timestamp)`, `tool_call_logs(tool_name)`

#### 3. 用户反馈信号检测 (`src/metrics.rs`)

基于关键词的中英文检测，无 LLM 开销：

| 信号类型 | 关键词 |
|---------|--------|
| Correction | "不对", "错了", "wrong", "no that's not", "that's incorrect" |
| Positive | "谢谢", "thanks", "perfect", "exactly", "great" |

在用户消息进入 agent loop 前检测，计入 `session_metrics.user_corrections`。

#### 4. Agent Engine 集成 (`src/agent_engine.rs`)

5 个退出点全部添加 `flush_session_metrics`：
- 正常返回（end_turn）
- 工具循环上限
- LLM 错误
- ACP 命令提前返回
- Memory 命令提前返回

关键修复：`clone→take` 顺序（先 clone metrics，再 take tool_calls），确保 tool_call_count 与 tool_call_logs 数量一致。

#### 5. Reflector 聚合 (`src/scheduler.rs`)

`aggregate_session_metrics()` 在每个 reflector 周期运行：
- 汇总近期会话数量、平均迭代次数、循环/溢出/纠正统计
- 标记成功率 <50% 的工具

### 验证

通过 Web API 发送测试消息，确认 `session_metrics` 和 `tool_call_logs` 表正确写入数据。`tool_call_count` 与 `tool_call_logs` 行数一致。

---

## Phase 1 — Skill 进化 (Skill Evolution)

**状态**：已完成 ✓  
**代码增量**：~900 行  
**新增文件**：`src/skill_evolution.rs`  
**修改文件**：`src/skills.rs`, `src/db.rs`, `src/agent_engine.rs`, `src/scheduler.rs`, `src/lib.rs`

### 实现内容

#### 1. 信任等级体系 (`src/skills.rs`)

```
Official (默认)  — 手写技能，随代码发布，所有工具权限
Verified         — 自动生成，经使用验证，只读工具
Candidate        — 自动生成，未验证，只读工具，禁止 bash/write_file/edit_file
Archived         — 已淘汰，不加载
```

`SkillMetadata` 和 `SkillFrontmatter` 扩展 `trust_level` 字段。技能发现扫描 `skills_dir` + `skills_dir/auto-generated/`，跳过 Archived 级别。技能目录中以 `[candidate]`/`[verified]` 标记非 Official 技能。

#### 2. DB Schema v5→v6

新增两张表：

| 表 | 字段 | 用途 |
|---|---|---|
| `skill_activations` | skill_name, chat_id, timestamp, success, tokens_used, duration_ms | 技能激活追踪 |
| `skill_generation_log` | pattern_hash, skill_name, generated_at, status | 已处理模式记录（防重复生成） |

`tool_call_logs` 新增 `session_metric_id` 列关联会话。

新增查询方法：
- `get_skill_health_since()` — 按技能聚合激活次数、成功率、平均 token、最近/首次激活时间
- `get_tool_call_sequences_since()` — 获取按会话分组的工具调用序列
- `pattern_already_processed()` / `log_pattern_processed()` — 模式去重
- `get_meta()` / `set_meta()` — 通用 KV 存储（用于节流控制）

#### 3. 技能激活记录 (`src/agent_engine.rs`)

当工具名为 `activate_skill` 时，从输入参数提取 `skill_name`，估算 token 消耗（result.content.len()/4），写入 `skill_activations` 表。

#### 4. 模式检测 (`src/skill_evolution.rs`)

```
detect_patterns(sequences, min_seq_len=3, min_occurrences=3)
  ├── extract_ngrams(): 从每个会话的工具序列提取 3~8 长度的 n-gram
  ├── 跨会话计数，会话内去重
  ├── 按出现次数降序排列
  └── 去除被更长模式包含的子模式
```

返回 `Vec<ToolPattern>`，每个包含 sequence、occurrences、hash、example_chat_ids。

#### 5. LLM 技能生成 (`src/skill_evolution.rs`)

```
generate_skill_content(llm, pattern) → SKILL.md 内容
  ├── 系统提示：生成 YAML frontmatter + 使用说明
  ├── 输入：工具序列、出现次数、示例 chat_id
  └── 约束：name kebab-case <30字符、描述触发条件、禁止危险工具
```

#### 6. 校验 + 写入

- `validate_candidate_content()` — 必须有 YAML frontmatter，body 中不得引用 bash/write_file/edit_file
- `extract_skill_name()` — 从 frontmatter 解析 name
- `write_candidate_skill()` — 写入 `skills/auto-generated/{name}/SKILL.md`

#### 7. 晋升管线

```
should_promote(health): total_activations ≥ 3 AND success_rate > 70%
promote_skill(): 重写 SKILL.md 中 trust_level: candidate → verified
```

#### 8. 淘汰管线

| 条件 | 动作 |
|------|------|
| >30 天未使用 | 直接归档 |
| <20% 成功率 且 ≥5 次激活 | LLM 重写尝试 → 校验 → 成功则重置为 candidate，失败则归档 |

归档过程：
1. 复制 SKILL.md 到 `skills/.archive/{name}/`
2. 生成 `.retirement_meta.json`（时间戳、原因、指标快照）
3. 删除原目录

LLM 重写：`rewrite_failing_skill()` 使用专门的系统提示，要求保持同名、重置为 candidate、不引用危险工具。

#### 9. 学习记录 (`learnings.jsonl`)

每次生成/晋升/重写/淘汰追加一条 JSONL：

```json
{"timestamp":"...","source":"skill_gen|skill_promote|skill_rewrite|skill_retire","title":"...","context":"...","takeaway":"...","confidence":0.8}
```

存储路径：`{data_dir}/learnings.jsonl`

#### 10. Reflector 集成 (`src/scheduler.rs`)

三个新函数挂载到 `run_reflector`，在 Phase 0 的 `aggregate_session_metrics` 之后执行：

```
run_reflector()
  ├── backfill_embeddings()           # 已有
  ├── archive_stale_memories()        # 已有
  ├── aggregate_session_metrics()     # Phase 0
  ├── aggregate_skill_health()        # Phase 1 — 日志输出技能健康度
  ├── detect_and_generate_skills()    # Phase 1 — 6h 节流，模式→生成→校验→写入→晋升
  ├── check_skill_retirements()       # Phase 1 — 闲置/失败→重写/归档
  └── reflect_for_chat() per chat     # 已有
```

`detect_and_generate_skills` 节流机制：通过 `db_meta` 表的 `last_skill_gen_run` key 记录上次运行时间，间隔 <6 小时则跳过。每次最多处理 3 个新模式。

### 测试覆盖

| 模块 | 新增测试 | 内容 |
|------|---------|------|
| `src/metrics.rs` | 11 个 | 指标收集、反馈信号检测、中英文关键词 |
| `src/skill_evolution.rs` | 10 个 | n-gram 提取、模式检测、名称解析、校验、晋升、归档、学习记录 |
| `src/skills.rs` | 2 个 | trust_level 解析、frontmatter 解析 |

总测试数：660（Phase 0 前 646）

### 完整生命周期

```
工具调用日志 (Phase 0)
  → Reflector 检测重复模式 (3+ tool 序列出现 3+ 次)
  → LLM 生成 candidate SKILL.md
  → 校验（无危险工具引用）
  → 写入 skills/auto-generated/
  → 用户使用 → skill_activations 记录
  → 晋升检查（≥3 次, >70% 成功）→ verified
  → 淘汰检查（>30d 闲置 或 <20% 成功）
    → 闲置 → 归档
    → 失败 → LLM 重写 → 成功则重置 candidate / 失败则归档
  → learnings.jsonl 记录每次事件
```

---

## 部署

- **Release binary** 构建通过，0 clippy warnings
- **三个服务** (finclaw, rayclaw, yiclaw) 已更新并重启运行
- DB schema 自动迁移至 v6

## 下一步

**Phase 2 — 源码自修改 (Source Evolution)**：基于 learnings.jsonl 积累的信号，实现 Assessment→Plan→Impl 三阶段流水线，以 PR 方式提交代码变更，所有变更需人工审批。
