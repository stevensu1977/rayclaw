---
name: coding-agent
description: Delegate coding tasks (refactoring, analysis, debugging, code generation) to an external AI coding agent (Claude Code, Kiro, etc.) via ACP. Use when the user asks you to perform programming tasks that require reading/writing files, running commands, or deep code analysis on the server.
---

# Coding Agent Skill

Use the `acp_coding` tool to delegate coding tasks to an external AI coding agent. This is a single unified tool that handles everything automatically — session management, notifications, and execution.

## When to use

- User asks to refactor, analyze, debug, or generate code
- User says "用 claude/kiro 帮我..."、"let claude do..."、"delegate to coding agent" etc.
- Tasks that require reading project files, running tests, or making code changes on the server

## How to use

Just call `acp_coding` with the task description. That's it.

```
acp_coding(message="Analyze the project architecture and generate a diagram")
```

The tool automatically:
- Creates a new session or reuses an existing one for this chat
- Sends immediate notification to the user ("Starting agent...")
- Executes the task and returns results

### Parameters

| Param | Required | Description |
|-------|----------|-------------|
| `message` | Yes | The coding task to execute |
| `agent` | No | Agent name, default "claude". Use "kiro" for Kiro CLI. |
| `workspace` | No | Working directory for the agent |
| `async` | No | Set true for long tasks (> 2 min). Returns job_id, result pushed to chat when done |
| `timeout_secs` | No | Max wait time in sync mode (default 300) |

### Sync mode (default) — for quick tasks

```
acp_coding(message="List the project directory structure", workspace="/path/to/project")
```

### Async mode — for long-running tasks

```
acp_coding(message="Refactor the payment module and add unit tests", workspace="/path/to/project", async=true)
```

Returns job_id immediately. Result is auto-pushed to chat when done.

## Tips

- Sessions are automatically reused within the same chat — no need to manage them
- Sessions auto-expire after 10 minutes of inactivity
- If the user asks to check a job, use `acp_job_status(job_id=...)`
- To manually end a session: `acp_end_session(session_id=...)`
