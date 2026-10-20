# Tool-Call Auto-Healing with Local/Fast Models

## 1. Overview & Motivation

In agentic workflows (such as Claude Code, Cursor, or OpenCode), agent tool executions frequently encounter trivial failures:
- Small typos or mismatched flags (e.g. `grep -rn` on BSD vs GNU, `cargo test --test fooo`).
- Missing or wrong relative file paths (e.g. `cat file.txt` vs `cat src/file.txt`).
- Misremembered subcommand syntax.

When a tool call fails, it causes significant inefficiencies:
1. **Context Pollution**: Massive error logs and stack traces are injected into the conversation context.
2. **Token Burn**: The frontier model (e.g., Claude 3.5 Sonnet / Opus) must consume these error tokens and spend another generation turn outputting a minor fix.
3. **Prompt Cache Invalidation**: Extended error back-and-forth disrupts prompt-caching prefixes.

This design introduces **Tool-Call Auto-Healing (`rtk heal`)**, an autonomous local repair loop at the client/hook layer that catches command failures, delegates correction to a fast or local model (e.g., Ollama `qwen2.5-coder:1.5b` or cheap cloud model like Gemini Flash / Claude Haiku), re-runs the corrected command, and returns the final successful execution cleanly to the frontier model in a single turn.

---

## 2. Architecture & Components

```
[Claude Code / Agent Client]
             │
             │ 1. PreToolUse hook rewrites eligible command:
             │    `cargo test test_fooo` ──> `rtk heal -- cargo test test_fooo`
             ▼
      [rtk heal Runner]
             │
             │ 2. Attempt 1: Run original command
             ├────────────────────────────────────────────────┐
             │ (exit code == 0)                               │ (exit code != 0)
             ▼                                                ▼
     [Output stdout/stderr]                      [Capture stderr, cwd, exit code]
     [Exit 0 immediately]                                     │
     (0ms overhead, no LLM call)                              │ 3. Query Local / Fast LLM
                                                              │    (Hard timeout: 3s)
                                                              ▼
                                                   [Safety Verification Gate]
                                                              │
                                                              │ 4. Attempt 2: Run healed command
                                                              ├───────────────────────────────┐
                                                              │ (exit code == 0)              │ (exit != 0 or timeout)
                                                              ▼                               ▼
                                                 [Print `[rtk: healed to <cmd>]`] [Print original stderr]
                                                 [Print healed output]            [Exit original exit code]
                                                 [Exit 0]                         (Fail-open cleanly)
```

### 2.1 Why the Client/Hook Layer (PreToolUse)?
- **Exit Code Authority**: The wrapper controls the subshell process. If attempt 2 passes, `rtk heal` exits with code 0. The agent client sees a successful tool execution and does not trigger error-recovery cascades.
- **Proxy Decoupling**: Does not require running the RTK proxy daemon; functions as a native, offline-capable CLI tool.
- **Cache Friendly**: Keeps the conversation history linear and clean.

---

## 3. Detailed Component Specifications

### 3.1 Subcommand: `rtk heal`
* **Invocation**: `rtk heal [--timeout-ms <ms>] [--model <model>] -- <command...>`
* **Execution Flow**:
  1. Spawns the original command using standard OS process execution.
  2. If the command exits with `0`, streams stdout/stderr and exits with `0`.
  3. If the command exits with non-zero:
     - Checks command eligibility.
     - Formats prompt with truncated stderr (last 40 lines / max 1KB).
     - Issues single-shot completion request to configured local/fast endpoint.
     - Validates returned command against safety rules.
     - Spawns healed command.
     - If healed command exits `0`, prints `[rtk: auto-corrected to `<cmd>`]\n` followed by healed output, and exits `0`.
     - If healed command fails, times out, or violates safety, prints original stderr and exits with original exit code.

### 3.2 Safety Envelope & Sanitization Gate
Autonomous re-execution strictly adheres to the following safety invariants:
1. **Eligibility Allowlist**:
   Only read, inspection, search, build, and test commands are eligible for auto-healing:
   - Search/Inspect: `grep`, `rg`, `find`, `cat`, `ls`, `file`, `stat`, `head`, `tail`.
   - Git read-only: `git status`, `git diff`, `git log`, `git show`, `git branch`.
   - Build/Test: `cargo test`, `cargo check`, `pytest`, `npm test`, `go test`, `make test`.
2. **Denylist / Exclusions**:
   Commands containing destructive operations immediately bypass healing and fail open:
   - File deletion / truncation: `rm`, `truncate`, `dd`, `mkfs`, `mv`.
   - Git destructive: `git reset --hard`, `git push`, `git clean -f`, `git rebase`.
   - Privileged operations: `sudo`, `su`, `chmod`, `chown`.
   - Dangerous redirections: `> /dev/*`, network piping (`curl ... | sh`).
3. **Binary Boundary Invariant**:
   The healed command must invoke the exact same base binary as the original command (e.g. `cargo test` can only heal to `cargo test ...`, never to an unrelated executable).

### 3.3 LLM Client & Prompting
* **Default Endpoint**: Local OpenAI-compatible server at `http://localhost:11434/v1` (Ollama default).
* **Model**: Default `qwen2.5-coder:1.5b` (or `llama3.2:3b`).
* **Environment Overrides**:
  - `RTK_HEAL_ENDPOINT`: Endpoint URL.
  - `RTK_HEAL_MODEL`: Model name.
  - `RTK_HEAL_API_KEY`: Bearer token (optional for local, required for cloud providers).
  - `RTK_HEAL_TIMEOUT_MS`: Milliseconds before timeout (default: 3000ms).
* **System Prompt**:
  ```text
  You are a deterministic CLI repair assistant. Fix the failed terminal command.
  Return ONLY the corrected command enclosed in <command>...</command> tags.
  Do not explain. Do not include markdown code blocks.
  ```
* **User Message**:
  ```text
  Directory: <cwd>
  Original Command: <original_command>
  Exit Code: <exit_code>
  Error Output:
  <truncated_stderr>
  ```

### 3.4 Hook Integration (`src/hooks/decision.rs` & `src/hooks/hook_cmd.rs`)
In Claude Code's `PreToolUse` hook handling:
- Reads `[heal]` configuration section in `~/.config/rtk/config.toml`.
- If `heal.enabled = true` and the command matches the eligibility allowlist:
  - Rewrites command to `rtk heal -- <command>`.
- If `heal.enabled = false` or command is ineligible:
  - Retains standard RTK rewrite or pass-through.

---

## 4. Error Handling & Invariants

1. **Fail-Open Invariant**:
   If any of the following occur:
   - LLM endpoint unreachable or connection refused
   - LLM generation exceeds 3 seconds
   - LLM response contains no valid `<command>...</command>` tag
   - Suggested command fails safety validation
   - Healed command execution exits with non-zero
   Then `rtk heal` MUST output the original command's stderr and exit with the original command's exit code.
2. **Single-Attempt Limit**:
   Exactly 1 repair round is permitted. No recursion or chained retry loops.
3. **Attribution & Transparency**:
   Healed output must always prepend `[rtk: auto-corrected to `<cmd>`]\n` so that the primary conversation model maintains situational awareness of what command actually produced the output.

---

## 5. Testing & Verification Plan

1. **Unit Tests**:
   - `test_safety_allowlist`: Validates that allowed binaries are accepted and destructive binaries/flags are rejected.
   - `test_binary_boundary`: Ensures that a proposed fix changing the base binary (e.g. `grep` -> `find`) is rejected.
   - `test_tag_parsing`: Verifies extraction of command strings across edge cases (whitespace, embedded tags, trailing text).
2. **Mock Integration Tests**:
   - `test_heal_success`: Spins up a mock HTTP server responding with `<command>echo fixed</command>`, runs a failing command, verifies exit code 0 and stdout formatting.
   - `test_heal_timeout`: Verifies that a 3.5s delay on mock server cleanly aborts and returns the original error within budget.
   - `test_pass_through_on_zero`: Confirms that an initially successful command makes 0 HTTP calls.
