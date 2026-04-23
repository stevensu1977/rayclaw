# NanoBot 混合自进化方案

> 结合 yoyo-evolve（源码自修改）和 Hermes Agent（记忆/技能层运行时进化）两种路线，
> 在 NanoBot 现有架构上实现安全的自主进化能力。

## 0. 设计哲学

```
                   ┌──────────────────────────────────────┐
                   │          NanoBot 自进化光谱            │
                   │                                      │
    安全 ◄─────────┼──────────────────────────────────────► 激进
                   │                                      │
  Layer 1: Memory  │  ██████████  (运行时, 每次对话后)       │
  Layer 2: Skills  │  ████████    (Dream 周期, 每8h)       │
  Layer 3: Config  │  ██████      (离线评估, 每日)          │
  Layer 4: Source  │  ████        (PR 提交, 人工审批)       │
                   └──────────────────────────────────────┘
```

**核心原则：越靠近内核（源码），安全门槛越高。**

- Layer 1-2：已有能力，增强即可（Hermes 路线）
- Layer 3-4：新增能力，需要安全网（yoyo 路线）

---

## 1. Layer 1 — Memory 自进化（已有，增强）

### 现状
NanoBot 的 Dream 机制已能在 Phase 1 分析历史、Phase 2 执行文件编辑。
`MEMORY.md` 通过 GitStore 版本控制，支持 `line_ages()` 做陈旧度检测。

### 增强点

#### 1.1 Memory 质量自评估（新增）

在 Dream Phase 1 末尾增加 **Memory Audit** 步骤：

```python
# dream_phase1.md 模板追加
## Memory Audit
Review MEMORY.md and score each section:
- **Accuracy**: Are facts still correct? (check against recent conversations)
- **Freshness**: Lines older than 30d without re-confirmation → mark [stale]
- **Density**: Are there redundant entries saying the same thing?
- **Gaps**: What does the user frequently ask about that isn't captured?

Output:
[MEMORY-SCORE] accuracy=0.85 freshness=0.70 density=0.90 gaps=["user's deployment preferences", "error handling style"]
```

**切入点**: `nanobot/agent/memory.py` Dream 类, `nanobot/templates/agent/dream_phase1.md`

#### 1.2 对话质量反馈闭环（新增）

借鉴 yoyo-evolve 的 self-testing —— 但不是测试代码，而是测试对话质量：

```python
class ConversationEvaluator:
    """Dream Phase 1 结束后运行，评估最近 N 轮对话"""
    
    SIGNALS = {
        "user_corrections": "用户纠正了 bot 的错误回答",
        "repeated_questions": "用户重复问同一个问题（说明记忆没生效）",
        "tool_failures": "工具调用失败次数",
        "long_turns": "单轮超过 5 次 iteration（说明 bot 在打转）",
        "explicit_feedback": "用户说'不对'/'错了'/'不是这个意思'",
    }
    
    def evaluate(self, sessions: list[Session]) -> EvalReport:
        # 统计信号 → 生成改进建议 → 写入 memory/learnings.jsonl
        ...
```

**切入点**: 新文件 `nanobot/agent/evaluator.py`，由 Dream 调用

---

## 2. Layer 2 — Skill 自进化（已有，增强）

### 现状
Dream Phase 2 已能通过 `write_file` 自动创建新 Skill。
`skills/skill-creator/SKILL.md` 提供了格式指导。

### 增强点

#### 2.1 Skill 使用率追踪 + 自动淘汰

```python
class SkillTracker:
    """跟踪每个 Skill 的激活次数、成功率、最后使用时间"""
    
    def record_activation(self, skill_name: str, success: bool):
        # 写入 workspace/skills/.metrics.jsonl
        ...
    
    def stale_skills(self, days: int = 30) -> list[str]:
        # 超过 N 天未使用的 Skill → 建议 Dream 归档
        ...
    
    def failing_skills(self, threshold: float = 0.3) -> list[str]:
        # 成功率低于阈值 → 建议 Dream 重写或删除
        ...
```

**切入点**: `nanobot/agent/skills.py` SkillsLoader, AgentHook `before_execute_tools`

#### 2.2 Skill 自动生成触发器

当 Dream 检测到以下信号时，自动尝试创建新 Skill：

| 信号 | 触发条件 | 示例 |
|------|---------|------|
| 重复模式 | 用户连续 3 次请求相同类型任务 | "又要查天气了" → 自动创建 weather skill |
| 工具组合模式 | 同一组工具调用反复出现 | web_search + web_fetch + summarize → 创建 research skill |
| 用户显式反馈 | "你应该记住怎么做这个" | 直接提取当前对话为 skill |
| 外部 Skill Registry | 定期扫描 vercel-labs/skills | 发现新的适用 skill → 建议安装 |

**切入点**: Dream Phase 1 分析 → Phase 2 执行 `write_file` 到 `workspace/skills/`

---

## 3. Layer 3 — Config 自调优（新增，中等风险）

### 设计

允许 bot 在 **受限范围内** 调整自己的运行时配置。

#### 3.1 可调参数白名单

```yaml
# 自进化可触及的配置项（白名单）
evolvable_config:
  allowed:
    - agents.defaults.context_window    # 根据使用情况调整
    - agents.defaults.max_iterations    # 根据任务复杂度调整
    - agents.defaults.dream.interval    # 根据对话频率调整
    - agents.defaults.session_ttl       # 根据用户使用模式调整
    - tools.exec.timeout                # 根据任务类型调整
  forbidden:
    - providers.*                       # 不能改 API keys
    - channels.*                        # 不能改通信渠道
    - gateway.*                         # 不能改网络配置
    - tools.my.allow_set                # 不能自己给自己提权
```

#### 3.2 Config Tuning Agent

```python
class ConfigTuner:
    """每日运行一次，分析运行指标，提出配置调整建议"""
    
    def analyze(self) -> list[ConfigProposal]:
        metrics = self.collect_metrics()  # 从 sessions/ 收集
        
        proposals = []
        
        # 示例规则：如果 80% 的对话都超过 max_iterations 的 70%，建议提高
        if metrics.avg_iterations > 0.7 * config.max_iterations:
            proposals.append(ConfigProposal(
                key="agents.defaults.max_iterations",
                current=config.max_iterations,
                proposed=config.max_iterations + 5,
                reason=f"Average {metrics.avg_iterations} iterations, hitting ceiling",
                confidence=0.8
            ))
        
        return proposals
    
    def apply(self, proposal: ConfigProposal):
        if proposal.confidence < 0.7:
            # 低置信度 → 仅记录建议，不自动应用
            self.log_suggestion(proposal)
        else:
            # 高置信度 → 自动应用 + Git commit + 24h 观察期
            self.apply_with_rollback_window(proposal)
```

#### 3.3 安全网（借鉴 yoyo）

```python
class ConfigSafetyNet:
    """配置变更的安全网"""
    
    def pre_check(self, proposal: ConfigProposal) -> bool:
        # 1. 白名单检查
        if proposal.key not in EVOLVABLE_CONFIG:
            return False
        
        # 2. 变更幅度检查（单次不超过 ±30%）
        if abs(proposal.proposed - proposal.current) / proposal.current > 0.3:
            return False
        
        # 3. 硬性边界检查
        if proposal.key == "max_iterations" and proposal.proposed > 50:
            return False
        
        return True
    
    def post_check(self, proposal: ConfigProposal, observation_hours: int = 24):
        """24h 后检查：应用后指标是否改善"""
        before_metrics = self.metrics_before(proposal.applied_at)
        after_metrics = self.metrics_after(proposal.applied_at)
        
        if after_metrics.error_rate > before_metrics.error_rate * 1.5:
            self.revert(proposal)
            self.file_learning(f"Config change {proposal.key} caused regression, reverted")
```

**切入点**: 
- 新文件 `nanobot/agent/config_tuner.py`
- 由 CronService 每日触发
- ConfigStore 需要支持 programmatic write（当前是 `config.json`，需要 `nanobot/config/loader.py` 增加 `save()` 方法）

---

## 4. Layer 4 — Source 自修改（新增，高风险，需安全网）

### 设计

这是最激进的一层。**不建议 bot 直接修改自己正在运行的代码**，而是采用 yoyo 的 **"提 PR + 测试验证"** 模式。

#### 4.1 架构

```
┌─────────────────────────────────────────────────────────┐
│                    Evolution Pipeline                    │
│                                                         │
│  ┌──────────┐   ┌──────────┐   ┌──────────┐            │
│  │Assessment│──▶│ Planning │──▶│  Impl    │            │
│  │  Agent   │   │  Agent   │   │  Agent   │            │
│  └──────────┘   └──────────┘   └──────────┘            │
│       │              │              │                    │
│       ▼              ▼              ▼                    │
│  assessment.md   task_*.md    git branch +              │
│                               pytest + PR               │
│                                                         │
│  ┌──────────────────────────────────────────────┐       │
│  │              Safety Net (外部)                │       │
│  │  • PROTECTED_FILES 不可修改                   │       │
│  │  • pytest 必须全部通过                        │       │
│  │  • PR 必须人工审批 (Layer 4 唯一强制要求)     │       │
│  │  • 单任务 revert + auto-issue                │       │
│  └──────────────────────────────────────────────┘       │
└─────────────────────────────────────────────────────────┘
```

#### 4.2 Evolution Session（每日/每周触发）

```python
class EvolutionSession:
    """源码自进化会话，借鉴 yoyo-evolve 的多阶段流水线"""
    
    PROTECTED_FILES = [
        "nanobot/agent/loop.py",         # 核心循环不可改
        "nanobot/config/schema.py",       # 配置 schema 不可改
        "nanobot/templates/SOUL.md",      # 身份不可改
        "nanobot/templates/agent/identity.md",  # 身份不可改
        "scripts/",                       # 运维脚本不可改
        ".github/",                       # CI 不可改
        "tests/conftest.py",             # 测试基础设施不可改
    ]
    
    MAX_FILES_PER_TASK = 3    # yoyo 的经验：每任务最多改 3 个文件
    MAX_TASKS_PER_SESSION = 3  # yoyo 的经验：每次最多 3 个任务
    
    async def run(self):
        session_sha = git_rev_parse("HEAD")
        
        # Phase 1: Assessment
        assessment = await self.assess(
            source_files=glob("nanobot/**/*.py"),
            recent_learnings=read("memory/learnings.jsonl", last=50),
            recent_sessions=self.recent_sessions(days=7),
            test_results=await run("pytest --tb=short"),
            conversation_eval=self.conversation_evaluator.report(),
        )
        
        # Phase 2: Planning
        tasks = await self.plan(
            assessment=assessment,
            priority=[
                "test_failures",          # P0: 测试失败
                "conversation_quality",    # P1: 对话质量问题
                "error_patterns",          # P2: 重复出现的错误
                "performance_bottlenecks", # P3: 性能瓶颈
                "code_quality",           # P4: 代码质量
            ]
        )
        
        # Phase 3: Implementation (per task, on a branch)
        branch = f"evolve/{date.today()}"
        git_checkout_b(branch)
        
        for task in tasks[:self.MAX_TASKS_PER_SESSION]:
            pre_sha = git_rev_parse("HEAD")
            
            try:
                await self.implement(task)
                self.verify_protected_files(pre_sha)
                await self.run_tests()
                await self.evaluate(task, pre_sha)  # 独立评估 agent
            except (ProtectedFileViolation, TestFailure, EvalFailure) as e:
                git_reset_hard(pre_sha)
                self.file_issue(task, e)
                self.record_learning(f"Task '{task.title}' reverted: {e}")
                continue
        
        # Phase 4: PR
        if git_rev_parse("HEAD") != session_sha:
            create_pr(
                branch=branch,
                title=f"[self-evolve] {date.today()}",
                body=self.generate_pr_description(),
                labels=["self-evolve", "needs-review"],
            )
```

#### 4.3 Protected File Guard（shell 层，不在 Python 内）

借鉴 yoyo-evolve 的做法 —— 安全网必须在 agent **外部**：

```bash
#!/bin/bash
# scripts/evolve_guard.sh — 在 agent 外部运行

PROTECTED_FILES=(
    "nanobot/agent/loop.py"
    "nanobot/config/schema.py"
    "nanobot/templates/SOUL.md"
    "nanobot/templates/agent/identity.md"
    "scripts/"
    ".github/"
)

check_protected() {
    local pre_sha=$1
    for pattern in "${PROTECTED_FILES[@]}"; do
        if git diff --name-only "$pre_sha"..HEAD | grep -q "^$pattern"; then
            echo "VIOLATION: Protected file modified: $pattern"
            git reset --hard "$pre_sha"
            return 1
        fi
    done
    return 0
}
```

#### 4.4 可进化区域（白名单）

| 目录 | 可改内容 | 风险 |
|------|---------|------|
| `nanobot/agent/tools/` | 新增/改进工具 | 低 |
| `nanobot/skills/` | 新增/改进内置 skill | 低 |
| `nanobot/utils/` | 工具函数优化 | 中 |
| `nanobot/providers/` | 新增/优化 LLM provider | 中 |
| `nanobot/channels/` | 新增/修复渠道适配 | 中 |
| `nanobot/agent/memory.py` | 记忆系统改进 | 中-高 |
| `nanobot/agent/context.py` | 上下文构建优化 | 高 |

---

## 5. 时间线 & 优先级

```
Phase 1 (Week 1-2): Layer 1 增强
  ├── Memory Audit 评分机制
  ├── ConversationEvaluator
  └── learnings.jsonl 反馈闭环

Phase 2 (Week 3-4): Layer 2 增强  
  ├── Skill 使用率追踪
  ├── Skill 自动淘汰/归档
  └── 重复模式 → 自动 Skill 生成

Phase 3 (Week 5-6): Layer 3 新增
  ├── ConfigTuner + 白名单
  ├── 24h 观察期 + 自动 revert
  └── 指标收集管线 (sessions → metrics)

Phase 4 (Week 7-10): Layer 4 新增
  ├── Evolution Pipeline (Assessment → Plan → Impl)
  ├── Protected File Guard (shell 层)
  ├── PR 工作流 + 人工审批
  └── Evaluator-Fix Loop (最多 10 轮)
```

---

## 6. 与 yoyo-evolve / Hermes 的对比

| 维度 | yoyo-evolve | Hermes | 本方案 (NanoBot) |
|------|-------------|--------|-----------------|
| **进化范围** | 源码 only | Memory + Skills only | 4 层递进 (Memory → Skills → Config → Source) |
| **触发方式** | 定时 cron (8h) | 运行时 + 离线 GEPA | 运行时(L1-2) + 每日(L3) + 每周(L4) |
| **安全网** | shell guard + cargo test | 人工 PR 审查 | 分层：L1-2 自动 / L3 自动+观察期 / L4 PR+人工 |
| **身份保护** | IDENTITY.md 不可变 | SOUL.md 不可变 | SOUL.md + identity.md + loop.py 不可变 |
| **回滚** | git reset per-task | 无明确回滚 | per-task revert + 24h config rollback + session-level revert |
| **学习积累** | learnings.jsonl + active_learnings.md | MEMORY.md 演化 | learnings.jsonl + MEMORY.md + Skill metrics + Config history |
| **人工干预** | 最少（仅 sponsor 机制） | PR 审查 | L1-3 自动 / L4 必须人工 |
| **语言** | Rust (cargo test 强保障) | Python (无类型安全) | Python (pytest + mypy + ruff 组合保障) |

---

## 7. 关键创新点

### 7.1 "温度梯度"安全模型

不是简单的"能改/不能改"二分法，而是 4 层温度梯度：
- **冷区** (L1-2): 自由进化，Git 版本控制即安全网
- **温区** (L3): 白名单 + 幅度限制 + 24h 观察期
- **热区** (L4): 独立分支 + 测试通过 + 人工审批
- **冻结区**: SOUL.md、loop.py、schema.py 永不可改

### 7.2 "吃自己的狗粮"评估

yoyo 用 `cargo test` 做硬性验证。我们的对话型 bot 没有这个奢侈 ——
但可以用 **Conversation Replay Test**：

```python
class ConversationReplayTest:
    """用历史对话做回归测试"""
    
    def run(self, golden_conversations: list[GoldenConversation]):
        for conv in golden_conversations:
            # 用修改后的代码重新处理历史输入
            new_response = self.agent.process(conv.input)
            
            # LLM 评估：新回答是否至少和旧回答一样好
            eval_result = self.evaluator.compare(
                original=conv.original_response,
                new=new_response,
                user_feedback=conv.user_feedback,  # 如果有的话
            )
            
            if eval_result.regression:
                raise RegressionDetected(conv, eval_result)
```

### 7.3 "进化压力"来源多样化

yoyo 靠 GitHub issues 和竞品对比。我们可以加入：

| 压力来源 | 实现 | 权重 |
|---------|------|------|
| 用户显式反馈 | 对话中的 "不对"/"错了" | 最高 |
| 工具失败率 | AgentHook 统计 | 高 |
| 重复问题检测 | Memory 比对 | 高 |
| 对话轮次膨胀 | Session metrics | 中 |
| 社区 Skill 更新 | 定期扫描 registry | 低 |
| 竞品能力对比 | 每周自动测试 | 低 |

---

## 8. 最小可行路径 (MVP)

如果只做一件事，做 **Layer 1.2 + Layer 2.1**：

```
ConversationEvaluator (评估对话质量)
  → 发现问题 (e.g. "用户连续纠正了3次关于K8s的回答")
  → Dream Phase 1 产出改进建议
  → Dream Phase 2 更新 MEMORY.md 或创建新 Skill
  → SkillTracker 追踪新 Skill 效果
  → 下一轮 Evaluator 验证是否改善
```

这是一个 **完整的反馈闭环**，不改源码、不碰配置、零风险，
但已经具备了"发现问题 → 尝试修复 → 验证效果"的进化能力。

**预估工作量**: ~3-5 天（主要是 ConversationEvaluator + SkillTracker + Dream 模板修改）
