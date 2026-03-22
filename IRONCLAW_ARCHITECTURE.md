# IronClaw Architecture Notes

This document explains:

- how channel messages enter IronClaw
- how they become conversational turns or background jobs
- how those jobs execute
- how tool, secret, and domain permissions are granted
- what you can do from a messaging channel like Slack vs what must be done via CLI, web UI, DB-backed settings, or env/config

It is based on the current implementation in this repository, not just the high-level docs.

## 1. End-to-end message flow

### 1.1 Channels normalize everything into `IncomingMessage`

All channels eventually emit `IncomingMessage` values through the shared `Channel` interface.

Relevant code:

- `src/channels/channel.rs`
- `src/channels/manager.rs`

Important `IncomingMessage` fields:

- `channel`: which channel produced the message
- `user_id`: the owner/scope used for session and persistence isolation
- `sender_id`: the raw channel actor
- `thread_id` / `conversation_scope_id`: conversation routing key
- `metadata`: channel-specific routing and auth details
- `attachments`: extracted files/media
- `is_internal`: reserved for in-process messages such as job-monitor injections

`ChannelManager` starts all channels, merges their streams, and also merges an internal injection stream used by background components. That means user-originated channel traffic and internally generated messages enter the same top-level agent loop.

### 1.2 Main wires channels into a single agent runtime

Startup happens in `src/main.rs` and `src/app.rs`:

- `AppBuilder` initializes DB, secrets, LLM, tools, workspace, extension manager, and session manager.
- `main.rs` constructs channels, webhook routes, the gateway, the scheduler, and the agent.
- `ChannelManager::start_all()` returns one merged stream.
- `Agent::run()` consumes that stream.

The important design point is that the web gateway is not a separate execution engine. It is another ingress/egress surface on top of the same agent, sessions, scheduler, tools, and history.

### 1.3 `Agent::handle_message()` is the central ingress point

Relevant code:

- `src/agent/agent_loop.rs`
- `src/agent/submission.rs`
- `src/agent/thread_ops.rs`

The high-level order is:

1. Internal messages are forwarded directly back out, bypassing the normal LLM/tool pipeline.
2. The message-tool context is set so outbound messaging tools know which channel/target they are operating on.
3. `SubmissionParser::parse()` classifies the message.
4. inbound hooks may modify or reject user input
5. historical threads may be hydrated from DB
6. the session and thread are resolved
7. if the thread is in auth mode, the next user message is intercepted as a secret token before it reaches history or the LLM
8. event-triggered routines may consume the message
9. the parsed submission is executed

Submissions split into two broad categories:

- control/system submissions: `/undo`, `/status`, `/model`, approvals, etc.
- normal user input: enters the conversational tool loop

### 1.4 Explicit commands and natural language split early

There are two parsers:

- `SubmissionParser` handles special control submissions
- `Router` handles explicit slash commands such as `/job`, `/status`, `/cancel`, `/list`

Natural language does not go through command routing. It becomes a normal conversational turn, and the LLM can decide to call tools such as `create_job`, `tool_install`, `tool_auth`, `http`, and so on.

That means jobs can be created in two ways:

- explicitly: `/job do X`
- implicitly: the model calls `create_job`

## 2. How a normal message becomes a turn

### 2.1 `process_user_input()` does the thread-level work

Relevant code:

- `src/agent/thread_ops.rs`

For normal conversational input, IronClaw:

1. checks thread state
2. rejects new input if the thread is already processing or awaiting approval
3. runs safety validation and secret-leak checks on the inbound text
4. auto-compacts context if needed
5. creates an undo checkpoint
6. augments the message with attachment-derived context
7. starts a new turn on the thread
8. persists the user message immediately
9. sends a thinking status update to the channel
10. enters the shared agentic loop

The turn state and session state are in-memory runtime objects, but messages and jobs are also persisted to the DB when configured.

### 2.2 The shared agentic loop is used everywhere

Relevant code:

- `src/agent/agentic_loop.rs`
- `src/agent/dispatcher.rs`
- `src/worker/job.rs`
- `src/worker/container.rs`

IronClaw uses the same core loop for:

- interactive chat turns
- background scheduler jobs
- sandbox/container workers

Conceptually the loop is:

1. call LLM
2. if text is returned, either continue or finish
3. if tool calls are returned, preflight them
4. execute tools
5. append sanitized tool results
6. loop again until completion or stop

The delegate changes by context:

- `ChatDelegate` for user chat
- `JobDelegate` for background jobs
- `ContainerDelegate` for sandbox/container runs

## 3. How messages become jobs

### 3.1 Explicit `/job`

Relevant code:

- `src/agent/router.rs`
- `src/agent/commands.rs`
- `src/agent/scheduler.rs`

`/job <description>` is routed to `MessageIntent::CreateJob`, then `handle_create_job()`, which calls `Scheduler::dispatch_job(...)`.

### 3.2 Implicit `create_job` tool calls

Relevant code:

- `src/tools/builtin/job.rs`
- `src/agent/scheduler.rs`
- `src/orchestrator/job_manager.rs`

The conversational model can also call `create_job`.

There are two execution paths:

- local scheduler path: uses `Scheduler::dispatch_job(...)`
- sandbox/container path: uses `ContainerJobManager` and creates a sandbox job record

The user does not need to know which one happened. The tool hides that choice.

### 3.3 What `dispatch_job()` actually does

Relevant code:

- `src/agent/scheduler.rs`

`dispatch_job()` is the preferred entry point because it:

1. creates a `JobContext` in `ContextManager`
2. applies metadata like token caps
3. persists the job to the database
4. computes an autonomous approval context
5. schedules worker execution

This is where a conversation-level request turns into a durable background unit of work.

## 4. How jobs execute

### 4.1 Scheduler jobs

Relevant code:

- `src/agent/scheduler.rs`
- `src/worker/job.rs`

For scheduler jobs:

- the scheduler creates a worker and an mpsc control channel
- the worker starts after receiving `WorkerMessage::Start`
- the worker loads the job context and runs the shared agentic loop
- tool execution inside jobs is governed by an `ApprovalContext`

Important difference from chat:

- jobs do not pause and wait for a human approval dialog
- instead, they get an explicit autonomous allowlist of tools
- if a tool is not allowed in that context, it is blocked

So interactive approvals and autonomous jobs are two different permission systems.

### 4.2 Container jobs

Relevant code:

- `src/worker/container.rs`
- `src/sandbox/`
- `src/orchestrator/`

Container jobs run inside Docker with a sandbox policy:

- `ReadOnly`
- `WorkspaceWrite`
- `FullAccess`

Network access is proxied unless `FullAccess` is used. The proxy enforces a domain allowlist.

Credentials are not dumped into the global process environment. They are fetched per job and injected into child processes for that job.

### 4.3 Claude Code bridge jobs

Relevant code:

- `src/worker/claude_bridge.rs`
- `src/config/sandbox.rs`

For Claude Code mode, IronClaw writes `.claude/settings.json` inside the job workspace with an explicit permission allowlist:

- `Read(*)`
- `Write(*)`
- `Edit(*)`
- `Glob(*)`
- `Grep(*)`
- `NotebookEdit(*)`
- `Bash(*)`
- `Task(*)`
- `WebFetch(*)`
- `WebSearch(*)`

Those defaults come from `ClaudeCodeConfig`.

This is separate from IronClaw’s own chat-tool approvals.

## 5. Permission model: there are multiple layers

This is the part that matters most operationally.

### 5.1 Layer A: per-invocation approval in a live conversation

Relevant code:

- `src/tools/tool.rs`
- `src/agent/dispatcher.rs`
- `src/agent/thread_ops.rs`
- `src/agent/session.rs`

Each tool invocation declares one of:

- `Never`
- `UnlessAutoApproved`
- `Always`

In interactive chat:

- `Never`: runs immediately
- `UnlessAutoApproved`: asks once, then can be remembered for the session if the user chooses "always"
- `Always`: asks every time

The pending approval is stored in the thread in memory as `PendingApproval`.

The user can answer with:

- `yes`
- `always`
- `no`

That works through normal message parsing, not only through the web UI.

Important limits:

- this approval state is in memory only
- restart clears pending approvals
- "always" only affects the current session’s `auto_approved_tools`
- it is not a durable global configuration grant

### 5.2 Layer B: session-level auto-approval

If the user responds with `always`, the tool name is added to `Session.auto_approved_tools`.

This is still conversational runtime state, not configuration.

It affects tools with `ApprovalRequirement::UnlessAutoApproved`, but not `Always`.

Example:

- regular `shell` commands are `UnlessAutoApproved`
- dangerous shell commands are `Always`

### 5.3 Layer C: global "skip approvals" switch

Relevant code:

- `src/config/agent.rs`
- `src/settings.rs`

`AGENT_AUTO_APPROVE_TOOLS=true` disables interactive approval checks entirely.

This is real configuration, not a chat response.

This setting is supported via DB-backed settings as `agent.auto_approve_tools`, and also via env var. Config precedence is:

- env
- TOML
- DB
- defaults

### 5.4 Layer D: whether chat can use local dev tools at all

Relevant code:

- `src/config/agent.rs`
- `src/app.rs`
- `src/tools/registry.rs`

This is the switch that controls whether the chat agent itself gets `shell`, `read_file`, `write_file`, `list_dir`, and `apply_patch`.

The key point:

- `allow_local_tools` is resolved from `ALLOW_LOCAL_TOOLS`
- it is not loaded from `settings.agent`
- in the current code, this is env-only runtime config
- storing a key like `agent.allow_local_tools` in the generic settings store does not make the resolver honor it

When `ALLOW_LOCAL_TOOLS=true`, `AppBuilder` registers dev tools into the main chat tool registry.

When it is false, those tools are not available to ordinary chat turns. They may still exist inside container jobs, because container workers separately call `register_container_tools()`.

This is the biggest answer to your `bash` question:

- granting chat access to `bash`/file-edit tools is not done by replying "yes" in Slack
- it is enabled by startup config, primarily `ALLOW_LOCAL_TOOLS=true`

### 5.5 Layer E: autonomous-job tool allowlists

Relevant code:

- `src/tools/tool.rs`
- `src/agent/scheduler.rs`
- `src/worker/job.rs`

Background jobs do not stop and ask the user.

Instead, they receive an `ApprovalContext::Autonomous { allowed_tools }`.

If a tool is not in that allowlist, the autonomous worker cannot use it.

This is different from interactive approvals and is generated by the scheduler.

### 5.6 Layer F: static capability restrictions for WASM tools/channels

Relevant code:

- `src/tools/README.md`
- `src/tools/wasm/capabilities.rs`
- `src/tools/wasm/allowlist.rs`
- `src/tools/wasm/storage.rs`
- channel/tool `*.capabilities.json` files

WASM tools and WASM channels declare capabilities in their capabilities JSON files:

- HTTP allowlist
- allowed secrets
- rate limits
- auth configuration
- setup-required secrets

This is a static capability declaration. It is not granted by chat approvals.

To change those capabilities, you generally:

- edit the tool/channel capabilities file
- reinstall/reload the extension if needed

## 6. Can I grant permissions from Slack or another messaging channel?

### 6.1 Yes, for live approval prompts

If the agent is waiting on a tool approval, a channel user can approve from chat with:

- `yes`
- `always`
- `no`

This is explicitly supported. `process_approval()` even searches other threads in the same session to handle channel/thread mismatches common in Slack/Telegram.

So:

- yes, you can approve or deny a pending tool call from a messaging channel
- yes, you can use `always` to auto-approve that tool for the current session

### 6.2 Yes, for extension/channel token submission after auth mode is entered

If `tool_auth` or `tool_activate` returns `awaiting_token`, the thread enters auth mode.

In that state, the next user message is intercepted by `process_auth_token()` and sent straight to the extension manager without entering:

- logs
- turn history
- normal LLM processing

So for many extension/channel credentials, you can paste the token in the messaging channel after the agent prompts for it.

That is a real secure conversational path in the current code.

### 6.3 No, not for core runtime config like `ALLOW_LOCAL_TOOLS`

For core runtime behavior such as:

- enabling chat-local `shell` / file-edit tools
- sandbox policy
- full-access opt-in
- extra sandbox network domains

you are dealing with startup/configuration state, not conversational approval state.

Those are granted through:

- env vars
- TOML config
- DB-backed settings
- CLI commands
- web settings/setup endpoints

not by a plain Slack message.

## 7. How to grant IronClaw access to `bash`, file tools, Codex, and other tools

This breaks into several different cases.

### 7.1 `bash`, `read_file`, `write_file`, `list_dir`, `apply_patch` in normal chat

These are the dev tools registered by `ToolRegistry::register_dev_tools()`.

To make them available in the main chat agent:

1. set `ALLOW_LOCAL_TOOLS=true`
2. restart IronClaw

Why restart matters:

- tool registration happens during app construction
- this is not dynamically toggled by a chat message

Operationally:

- use env/config management for `ALLOW_LOCAL_TOOLS`
- do not expect a Slack reply to permanently grant these tools

### 7.2 `bash` and file tools inside container jobs

Container workers always register container tools independently of the main chat tool registry.

That means:

- chat may not have `shell`
- a background/container job may still have `shell`

What controls safety there is the sandbox policy and autonomous allowlist, not `ALLOW_LOCAL_TOOLS`.

### 7.3 Claude Code tool access inside Claude bridge jobs

Claude Code jobs use the `ClaudeCodeConfig.allowed_tools` allowlist, which becomes `.claude/settings.json` in the job workspace.

To change that allowlist, use config/env for Claude Code settings, not chat approval.

### 7.4 Codex as an LLM backend

In this codebase, "Codex" is primarily an LLM/auth backend, not a chat tool named `codex`.

Relevant code:

- `src/main.rs`
- `src/llm/CLAUDE.md`
- `src/llm/openai_codex_session.rs`
- `src/llm/codex_auth.rs`

To grant IronClaw access to OpenAI Codex:

1. authenticate with `ironclaw login --openai-codex`
2. set `LLM_BACKEND=openai_codex`
3. optionally set Codex-related env vars such as model/session-path overrides

That access is granted through provider auth/session config, not through Slack approvals.

Separately, IronClaw can also read Codex CLI auth from `~/.codex/auth.json` when configured to do so.

### 7.5 WASM tools, MCP tools, and extensions

To grant access to additional capabilities:

- install the extension with `ironclaw tool install ...` or via the web extensions API/UI
- authenticate it with `ironclaw tool auth <name>` or the extension setup/auth UI
- activate it

If the extension requires manual token entry, the secure token-submission path can also happen through auth mode in chat as described above.

### 7.6 Secrets for jobs

Sandbox jobs can receive specific secrets through `create_job` credential grants:

- the secret must already exist in the secrets store
- the job request maps secret name to target env var

This does not give the agent every secret globally. It grants selected secrets to that job only.

## 8. How to grant access to domains for web requests

This also splits into multiple systems.

### 8.1 Built-in `http` tool in ordinary chat

Relevant code:

- `src/tools/builtin/http.rs`

The built-in orchestrator `http` tool does not use the sandbox proxy allowlist.

Instead, it enforces its own safety rules:

- HTTPS only
- public hosts only
- no localhost/private IPs/cloud metadata endpoints

Approval behavior is based mostly on HTTP method and credential usage:

- unauthenticated `GET` is `Never`
- credentialed requests and non-GETs are `UnlessAutoApproved`

So for plain chat `http` requests, there is not a user-maintained domain allowlist in the same sense as the sandbox/WASM systems.

### 8.2 Sandbox/container network allowlist

Relevant code:

- `src/config/sandbox.rs`
- `src/sandbox/config.rs`
- `src/sandbox/proxy/allowlist.rs`

Container jobs route network traffic through a proxy with a domain allowlist.

The allowlist is:

- the built-in default allowlist from `src/sandbox/config.rs`
- plus `SANDBOX_EXTRA_DOMAINS`
- or the DB-backed settings equivalent `sandbox.extra_allowed_domains`

This is the main answer if you want to allow container jobs to reach additional domains.

Ways to grant those domains:

- env var: `SANDBOX_EXTRA_DOMAINS=example.com,api.example.com`
- DB-backed setting: `sandbox.extra_allowed_domains` as a JSON array
- web settings API/UI
- CLI config command

Examples:

- CLI: `ironclaw config set sandbox.extra_allowed_domains '["example.com","api.example.com"]'`
- web API: `PUT /api/settings/sandbox.extra_allowed_domains`

This is configuration, not conversational approval.

### 8.3 WASM tool/channel HTTP allowlist

Relevant code:

- `src/tools/wasm/allowlist.rs`
- `src/tools/wasm/capabilities_schema.rs`
- `src/channels/wasm/schema.rs`

WASM HTTP access is driven by the extension capability file, for example:

```json
{
  "http": {
    "allowlist": [
      { "host": "api.slack.com", "path_prefix": "/api/", "methods": ["GET", "POST"] }
    ]
  }
}
```

This allowlist can also constrain:

- path prefix
- HTTP method

If you need to grant a new domain to a WASM tool or channel, you change the capabilities file and reload/reinstall the extension.

That is not something a Slack "yes" can do.

## 9. Practical answer: where to grant what

### 9.1 Things you can grant from a messaging channel

- approve a pending tool call with `yes`
- auto-approve a tool for the current session with `always`
- deny with `no`
- submit an extension/channel token after IronClaw enters auth mode

### 9.2 Things you generally grant via `ironclaw` commands

- install tools/extensions: `ironclaw tool install`
- authenticate tools/extensions: `ironclaw tool auth`
- configure setup secrets: `ironclaw tool setup`
- configure DB-backed settings: `ironclaw config set ...`
- authenticate OpenAI Codex backend: `ironclaw login --openai-codex`

### 9.3 Things you grant via web UI or web API

- extension setup secrets
- extension auth flow in gateway mode
- DB-backed settings like `agent.auto_approve_tools` or `sandbox.extra_allowed_domains`
- approve/deny pending tool calls from the web approval endpoint/UI

Important caveat:

- the generic settings API can write arbitrary keys, but chat-local dev tools are still controlled by env-only `ALLOW_LOCAL_TOOLS` in the current resolver, so treating web settings as the source of truth for that flag will not work today

### 9.4 Things you grant via env vars or config files

- `ALLOW_LOCAL_TOOLS=true` for chat-local bash/file tools
- `SANDBOX_POLICY=...`
- `SANDBOX_ALLOW_FULL_ACCESS=true`
- `SANDBOX_EXTRA_DOMAINS=...`
- `LLM_BACKEND=openai_codex`
- Codex/LLM provider env vars
- TOML overlays for durable startup config

## 10. Direct answers

### Can I grant permissions via Slack?

Yes, but only for live conversational approval and auth flows:

- approve/deny a pending tool call
- say `always` for current-session auto-approval
- submit a token when the thread is in auth mode

No, not for core runtime capability grants like enabling chat-local `bash` tools or changing sandbox domain allowlists.

### Do I have to use `ironclaw` commands or edit config files?

For real runtime configuration, yes. Use one of:

- `ironclaw config set ...`
- `ironclaw tool ...`
- web settings/setup endpoints
- env vars
- TOML config

Which one you need depends on the permission:

- chat-local dev tools: env/config
- extension auth/setup: CLI or web UI, optionally token submission in chat auth mode
- sandbox extra domains: config/env/web settings
- Codex backend access: `ironclaw login --openai-codex` plus backend config

### How do I grant access to `codex`, `bash`, and other tools?

- `codex` backend: authenticate with `ironclaw login --openai-codex`, then select `LLM_BACKEND=openai_codex`
- chat `bash` and file-edit tools: set `ALLOW_LOCAL_TOOLS=true` and restart
- container-job `bash`: enabled by worker/container tool registration and governed by sandbox policy
- Claude Code tool access: adjust Claude Code allowed-tool config
- WASM/MCP tools: install/auth/setup/activate via extension workflows

### How do I grant access to certain domains for web requests?

- built-in chat `http` tool: no operator-managed domain allowlist; it uses built-in safety restrictions
- sandbox/container web access: add domains to `SANDBOX_EXTRA_DOMAINS` or `sandbox.extra_allowed_domains`
- WASM tools/channels: edit the extension `*.capabilities.json` HTTP allowlist

## 11. Short operational checklist

If your goal is "let my IronClaw agent use bash in chat":

1. set `ALLOW_LOCAL_TOOLS=true`
2. restart IronClaw
3. if you want no prompts, also set `AGENT_AUTO_APPROVE_TOOLS=true` or approve per-session with `always`

If your goal is "let container jobs call api.example.com":

1. set `sandbox.extra_allowed_domains` to include `api.example.com`, or set `SANDBOX_EXTRA_DOMAINS`
2. restart if you changed env/TOML

If your goal is "let a WASM Slack/Notion/GitHub extension call a new endpoint":

1. edit that extension’s capabilities file
2. add the HTTP allowlist entry
3. reinstall/reload/activate the extension

If your goal is "let IronClaw use OpenAI Codex as the model backend":

1. run `ironclaw login --openai-codex`
2. set `LLM_BACKEND=openai_codex`
3. restart if needed
