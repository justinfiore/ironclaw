# IronClaw Troubleshooting: Slack-Driven Coding in the `ironclaw` Repo

This document analyzes how to get an IronClaw agent, driven over Slack, to work on a checked-out repository at:

`/home/openclaw/code/ironclaw`

with two distinct execution models:

1. IronClaw doing the coding itself through its own LLM/tool loop
2. IronClaw delegating to the `codex` CLI

It also explains the current failure mode:

```text
Worker failed for job ...: LLM error: Provider proxy request failed:
LLM proxy request failed: LLM tool complete: orchestrator returned 502 Bad Gateway
```

and clears up the "WASM vs Docker" confusion.

## 1. Short answer

Your current 502 is almost certainly **not** a bind-mount problem and **not** a container-to-orchestrator reachability problem.

Why:

- the worker started
- it reached the orchestrator at `http://172.17.0.1:50051`
- it fetched the job successfully
- it then failed on the **first proxied tool-calling LLM request**

That means the failure is happening in the host-side orchestrator's LLM provider path, specifically in:

- `POST /worker/{job_id}/llm/complete_with_tools`

The repo mount may still need refinement for real coding tasks, but it is not what produced this specific 502.

## 2. What is actually happening in your current setup

### 2.1 The project mount model

The sandbox job path accepted by IronClaw must live under:

`~/.ironclaw/projects/`

Relevant implementation:

- `src/tools/builtin/job.rs`
- `src/orchestrator/job_manager.rs`

Your bind mount:

```bash
mount --bind /home/openclaw/code/ironclaw /home/openclaw/.ironclaw/projects/ironclaw
```

is aligned with the current path validation model.

Inside the Docker worker container, that host path is mounted as:

- `/workspace`

So once the job starts, the worker should operate on:

- `/workspace`

not the original host path.

For example, the correct command inside the container is:

```bash
ls -la /workspace
```

not:

```bash
ls -la /home/openclaw/.ironclaw/projects/ironclaw
```

### 2.2 The orchestrator network path

Relevant implementation:

- `src/orchestrator/api.rs`
- `src/orchestrator/job_manager.rs`
- `src/NETWORK_SECURITY.md`

On Linux, worker containers talk back to the host orchestrator through:

- `172.17.0.1:<ORCHESTRATOR_PORT>`

defaulting to:

- `172.17.0.1:50051`

So your firewall rule is consistent with the current design:

```bash
sudo ufw allow in on docker0 from 172.17.0.0/16 to any port 50051 proto tcp comment 'Allow docker containers to talk to Ironclaw gateway'
```

One wording fix:

- this is not the web gateway
- it is the **orchestrator internal API**

### 2.3 Why the current failure points away from mount/firewall

The worker log sequence shows:

1. worker starts
2. worker reaches orchestrator
3. worker fetches job description
4. worker then makes a proxied LLM tool-completion call
5. orchestrator returns 502

That means:

- job fetch succeeded
- bearer-token auth succeeded
- docker bridge connectivity succeeded
- the failure is later, in the host-side `state.llm.complete_with_tools(...)` call

The relevant code path is:

- worker: `src/worker/proxy_llm.rs`
- HTTP client: `src/worker/api.rs`
- orchestrator handler: `src/orchestrator/api.rs`

In `src/orchestrator/api.rs`, a 502 is returned when the host-side LLM provider errors during `complete_with_tools`.

## 3. Immediate diagnosis of the 502

### 3.1 What the 502 means in code terms

The worker is calling:

- `POST /worker/{job_id}/llm/complete_with_tools`

The orchestrator handler does:

- build a `ToolCompletionRequest`
- call `state.llm.complete_with_tools(...)`
- on error, log it and return `502 Bad Gateway`

So the real error is on the host side and should appear in the host logs as something like:

```text
LLM tool completion failed for job <id>: <real provider error>
```

That host-side error is the one you need next.

### 3.2 Most likely root causes

Ordered by likelihood:

1. The host LLM backend is misconfigured or unauthenticated.
2. The host LLM backend supports plain completion but fails on tool-calling.
3. Codex-backed auth on the host is expired or unreadable.
4. The selected provider/model combination is incompatible with the tool schema path being used.
5. The host process cannot reach the upstream LLM endpoint.

The important part:

- the worker container does **not** need direct LLM credentials in worker mode
- the host orchestrator owns the real LLM provider

### 3.3 What to check first

Check the host log around the same timestamp for a line from:

- `src/orchestrator/api.rs`

specifically:

```text
LLM tool completion failed for job ...
```

That error will tell you whether the real failure is:

- auth
- expired token
- unsupported model/tool call path
- network outage
- provider-side error

## 4. Two different ways IronClaw can "code"

These are easy to conflate, but they are different.

### 4.1 Mode A: IronClaw worker/sub-agent does the coding itself

This is the standard `worker` sandbox job mode.

Relevant code:

- `src/worker/mod.rs`
- `src/worker/container.rs`
- `src/worker/proxy_llm.rs`
- `src/tools/registry.rs`
- `Dockerfile.worker`

What happens:

- a Docker container runs `ironclaw worker`
- the worker uses `ProxyLlmProvider`
- every LLM request is proxied back to the host orchestrator
- the worker has container-local coding tools:
  - `shell`
  - `read_file`
  - `write_file`
  - `list_dir`
  - `apply_patch`

This is the path you need for:

- reading the repo
- writing code
- running `git`
- running `./scripts/build-all.sh`

provided that:

- the repo is mounted to `/workspace`
- the host LLM provider is healthy
- the container user can write to the mounted files

### 4.2 Mode B: IronClaw delegates to Claude Code CLI

This is `claude_code` sandbox job mode.

Relevant code:

- `src/worker/claude_bridge.rs`
- `src/orchestrator/job_manager.rs`
- `src/config/sandbox.rs`
- `Dockerfile.worker`

What happens:

- a Docker container runs `ironclaw claude-bridge`
- the bridge spawns the `claude` CLI
- job prompts are fed into Claude Code
- Claude Code events are streamed back to IronClaw

This mode is first-class in the repo today.

### 4.3 There is no first-class `codex` CLI bridge today

This is the most important architectural point for your goal.

There is support for:

- Codex as an **LLM backend**
- Codex auth reuse from `~/.codex/auth.json`

There is **not** a built-in equivalent of `claude-bridge` for spawning the `codex` CLI.

Relevant evidence:

- the worker image installs Claude Code CLI in `Dockerfile.worker`
- there is no corresponding installation of a `codex` CLI
- there is no `codex_bridge.rs`
- the only Codex-specific code is in the LLM/auth layer

So if you want "delegate to `codex` CLI", that is a custom extension to the current architecture, not a built-in mode.

## 5. What "use Codex" can mean in this codebase

There are two different meanings.

### 5.1 Use Codex as IronClaw's LLM backend

Relevant code:

- `src/llm/CLAUDE.md`
- `src/config/llm.rs`
- `src/llm/codex_auth.rs`
- `src/main.rs`

This is already supported.

You can configure the host IronClaw process to use Codex-backed auth and model selection. Then worker containers will automatically benefit, because they proxy LLM calls back to the host.

That means:

- if your goal is "the worker should reason with Codex and then use shell/file tools to modify the repo"
- you do **not** need `codex` installed in the container
- you need the **host** IronClaw process configured to use the Codex-backed LLM provider

Practical configuration options:

- `ironclaw login --openai-codex`
- `LLM_BACKEND=openai_codex`

or, if you want to reuse the Codex CLI auth file directly on the host:

- `LLM_USE_CODEX_AUTH=true`
- optionally `CODEX_AUTH_PATH=/path/to/auth.json`

In this model, `~/.codex/auth.json` only needs to be readable by the **host** IronClaw process.

### 5.2 Use the actual `codex` CLI binary inside a job container

This is not first-class today.

To make this work, you would need a custom solution such as:

1. install the `codex` CLI into the worker image
2. make auth material available inside the container
3. decide how IronClaw invokes it

Invocation options:

- simplest: let the worker call `codex ...` through the existing `shell` tool
- better long-term: add a dedicated `codex_bridge` mode analogous to Claude Code

## 6. Recommended path for your immediate goal

If your first real goal is:

- from Slack, ask IronClaw to inspect and modify the `ironclaw` repo

then the fastest path is:

### Path 1: get standard `worker` mode healthy first

Do this before trying `codex` CLI delegation.

Why:

- `worker` mode already has shell/file tools
- `Dockerfile.worker` already includes `git`, `build-essential`, `node`, `npm`, `python3`, `gh`, and Rust tooling
- it is the shortest path to `ls`, `git status`, `cargo test`, and `./scripts/build-all.sh`

Your current blocker is the host-side LLM 502, not repo visibility.

### Path 2: once `worker` mode works, decide whether you still need actual `codex` CLI delegation

You may find that "IronClaw worker + Codex-backed host LLM" is enough.

That gives you:

- Codex reasoning
- IronClaw tool orchestration
- normal shell/file operations in the mounted repo

without needing to customize the worker image for a `codex` binary.

## 7. Concrete plan to get `worker` mode working on this repo

### 7.1 Make the mount target explicit

Use:

- host project path: `/home/openclaw/.ironclaw/projects/ironclaw`
- container working path: `/workspace`

When creating the job, make sure the job uses:

```text
project_dir=/home/openclaw/.ironclaw/projects/ironclaw
```

The create-job tool requires the host path to already exist and to live under `~/.ironclaw/projects/`.

### 7.2 Prompt the agent to operate on `/workspace`

For testing, do not ask it to use the host path from inside the container.

Use tasks like:

```text
Run `pwd`, `ls -la /workspace`, and `git status` in /workspace.
```

not:

```text
Run `ls -la /home/openclaw/.ironclaw/projects/ironclaw`.
```

### 7.3 Verify host-side LLM tool-calling outside the worker path

Because the 502 is in `complete_with_tools`, verify the host IronClaw process can successfully do a tool-calling turn before involving Docker.

Examples:

- ask the main agent to use any safe built-in tool
- verify the selected backend/model can complete with tools

If that fails outside Docker too, the issue is purely in the host LLM/provider configuration.

### 7.4 Check the real host-side error

Look for the host log line emitted by the orchestrator:

```text
LLM tool completion failed for job <id>: <real error>
```

Do not proceed to mount/auth image work until you have that error string.

### 7.5 Verify host-side Codex auth if using Codex as the LLM backend

If the host is meant to use Codex auth:

- verify `~/.codex/auth.json` exists and is readable by the host IronClaw process
- if using the auth.json reuse path, set `LLM_USE_CODEX_AUTH=true`
- if using the dedicated OpenAI Codex flow, run `ironclaw login --openai-codex`

Again, this is a **host** concern for worker mode.

### 7.6 Verify container write permissions on the mounted repo

The worker container runs as:

- UID 1000
- user `sandbox`

Relevant code:

- `Dockerfile.worker`
- `src/orchestrator/job_manager.rs`

So the host bind-mounted directory must be writable by UID 1000, or by a group/mode that allows writes from UID 1000.

Check:

```bash
id -u openclaw
stat -c '%u %g %A %n' /home/openclaw/code/ironclaw
stat -c '%u %g %A %n' /home/openclaw/.ironclaw/projects/ironclaw
```

If host ownership does not line up with UID 1000 or group permissions are too tight, you may later hit write failures even after the 502 is fixed.

## 8. Running build scripts and git commands

Once `worker` mode is healthy and the mount is writable, the stock worker image is already reasonably capable.

The current `Dockerfile.worker` includes:

- `git`
- `build-essential`
- `pkg-config`
- `libssl-dev`
- `nodejs`
- `npm`
- `python3`
- `python3-pip`
- `python3-venv`
- `gh`
- Rust toolchain

So the following are realistic targets in worker mode:

- `ls -la /workspace`
- `git status`
- `cargo build`
- `cargo test`
- `./scripts/build-all.sh`

Assuming, of course:

- the script exists in the mounted checkout
- the shebang is valid
- the repo’s own external dependencies are available

## 9. If you want actual `codex` CLI delegation

This requires additional work.

## 9.1 Current status

Stock IronClaw does **not** currently provide:

- a `codex` bridge mode
- a `codex` binary in `Dockerfile.worker`
- a helper equivalent to Claude Code's auth-copy logic for `~/.codex`

So "delegate to codex cli" currently means custom engineering.

## 9.2 Minimal viable approach

The simplest path is:

1. build a custom worker image based on `Dockerfile.worker`
2. install the `codex` CLI in that image
3. invoke `codex` through the existing `shell` tool

That lets the worker say things like:

```bash
cd /workspace && codex <args>
```

This is not elegant, but it avoids having to add a new bridge mode immediately.

## 9.3 Auth strategy for `codex` CLI with subscription OAuth

You said you log in to Codex using subscription OAuth.

There are two different auth-reuse patterns:

### Pattern A: host IronClaw reuses `~/.codex/auth.json` for LLM auth

This is already supported.

Use this if you want Codex only as the host LLM backend.

In this case:

- do **not** mount `~/.codex` into the worker container
- make sure the **host** process can read it

### Pattern B: actual `codex` CLI runs inside the container

Then the container itself needs Codex auth material.

The safest pattern is the same one already used for Claude Code:

1. bind mount host `~/.codex` read-only into a staging path such as:
   - `/home/sandbox/.codex-host:ro`
2. copy that directory into a writable path at container startup:
   - `/home/sandbox/.codex`

Why copy instead of direct bind-mount:

- OAuth clients often want to write refreshed auth state
- a writable live bind mount back to the host is higher risk
- the Claude bridge already uses the copy-into-writable-home pattern for `~/.claude`

Current limitation:

- there is no built-in `.codex` copy helper today
- you would need to add one

## 9.4 Recommended implementation for a real `codex` bridge

If you want this to be a first-class IronClaw capability, the clean design is:

1. add `codex` installation to `Dockerfile.worker`
2. add config for Codex CLI auth sourcing
3. add a `codex_bridge.rs`
4. mirror the Claude bridge design:
   - copy auth from read-only host mount into writable home
   - write explicit permission config if Codex supports it
   - stream events back through the orchestrator

That keeps `codex` CLI delegation symmetrical with Claude Code delegation.

## 10. What runs in WASM vs what runs in Docker

This is the clean mental model:

### 10.1 WASM tools and WASM channels

Relevant code:

- `src/tools/wasm/`
- `src/channels/wasm/`

These do **not** run in Docker.

They run:

- inside the main IronClaw process
- under a Wasmtime sandbox

Use cases:

- extension-style tools
- extension-style channels
- capability-declared HTTP access
- capability-declared secret access

Examples:

- Slack WASM channel
- custom WASM tools

### 10.2 Docker worker containers

Relevant code:

- `src/orchestrator/`
- `src/worker/`
- `Dockerfile.worker`

These are full Docker containers used for:

- `worker` sandbox jobs
- `claude_code` jobs

These are the containers you care about for repo coding tasks.

### 10.3 `src/sandbox/` is related but not the same as orchestrator job containers

This is another subtle but important distinction.

Relevant code:

- `src/sandbox/`
- `src/orchestrator/job_manager.rs`

The `src/sandbox/` subsystem is the generic Docker sandbox runner used for command execution and proxy-based network control.

The orchestrator job containers used by `create_job(... mode=worker/claude_code ...)` are a separate path managed by `ContainerJobManager`.

That means:

- "WASM" is not Docker
- "orchestrator worker container" is Docker
- "`src/sandbox/` proxy-limited command runner" is also Docker, but it is a different subsystem from the long-lived orchestrator job container path

This difference matters because the network and auth behavior are not identical between those paths.

## 11. Recommended staged rollout

### Stage 1: make standard worker coding work

Goal:

- Slack asks IronClaw to inspect and edit `/workspace`

Do this:

1. keep the bind mount under `~/.ironclaw/projects/ironclaw`
2. ensure the job uses that exact `project_dir`
3. prompt operations against `/workspace`
4. fix the host-side LLM 502 first
5. verify UID 1000 write access

### Stage 2: switch the host LLM to Codex if desired

Goal:

- IronClaw worker reasoning is powered by Codex-backed host auth

Do this on the host:

1. `ironclaw login --openai-codex` or enable `LLM_USE_CODEX_AUTH=true`
2. set `LLM_BACKEND=openai_codex` if appropriate
3. confirm the main agent can complete tool-calling turns

At this stage, the worker container still does not need the `codex` CLI binary.

### Stage 3: only if necessary, add real `codex` CLI delegation

Goal:

- the container can literally run `codex`

Do this:

1. extend `Dockerfile.worker` to install `codex`
2. add `.codex` auth mount/copy support
3. invoke `codex` via shell first
4. optionally later build a dedicated `codex_bridge`

## 12. What I would do next

In order:

1. Find the host log line:
   - `LLM tool completion failed for job 998a576f-cb34-4dc7-87e8-c765f88df74a: ...`
2. Fix that host-side LLM/provider issue first.
3. Re-run with an explicit task:
   - `Run pwd, ls -la /workspace, and git status in /workspace.`
4. Verify host-file writeability by UID 1000.
5. Once standard worker mode works, decide whether Codex-as-LLM is sufficient.
6. Only then invest in actual `codex` CLI delegation.

## 13. Bottom line

For "IronClaw over Slack can read/write the ironclaw repo", the built-in path is:

- Docker `worker` mode
- repo bind-mounted under `~/.ironclaw/projects/...`
- mounted into the container as `/workspace`
- host-side LLM provider handling all model calls

For "use Codex for reasoning", the easiest path is:

- configure the **host** IronClaw LLM backend to use Codex auth/provider

For "use the actual `codex` CLI inside the container", the current repo does not provide that as a first-class feature yet. That requires a custom worker image and auth-mount strategy, or a new bridge implementation.
