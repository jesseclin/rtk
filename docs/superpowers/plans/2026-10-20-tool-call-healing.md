# Tool-Call Auto-Healing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `rtk heal` and `PreToolUse` hook integration to intercept failed CLI tool calls, delegate repair to a fast/local LLM within a 3-second budget, re-execute verified fixes, and return a clean single-turn result.

**Architecture:** A standalone `rtk heal -- <command...>` subcommand executes the target command via `core::shell::command_from_args`. On exit code 0 it passes output through with zero LLM overhead; on non-zero exit code for an allowlisted command, it queries an OpenAI-compatible `/chat/completions` endpoint via `ureq`, validates the `<command>...</command>` response against strict binary-boundary and flag-safety rules, runs the healed command once, and falls open to the original stderr and exit code on any error or timeout.

**Tech Stack:** Rust (Edition 2024), `clap` 4, `ureq` 2 (synchronous HTTP with timeout), `serde` / `serde_json`, `toml`.

**Spec:** `docs/superpowers/specs/2026-10-20-tool-call-healing-design.md`

## Global Constraints

- **Test Isolation (`src/core/test_isolation/mod.rs`):** NEVER call `std::env::var`, `std::env::var_os`, or `std::env::current_dir` directly in `src/`. Always use `crate::core::user_env::var("<LITERAL>")` and `crate::core::user_dirs::current_dir()`.
- **No Async Runtime in CLI:** The root `rtk` crate does not link `tokio`; use `ureq` for synchronous HTTP requests with explicit timeouts.
- **Fail-Open Invariant:** Any network failure, timeout (>3000ms default), missing `<command>` tag, safety rejection, or non-zero exit on the second attempt MUST emit the original command's stdout/stderr and exit with the original command's exit code.
- **Single-Attempt Limit:** Exactly 1 repair round is permitted; never loop or recurse.
- **Attribution Banner:** Healed output MUST prepend `[rtk: auto-corrected to \`<cmd>\`]\n` to stdout before the healed command's output.
- **Hook Integrity Whitelist:** `Commands::Heal` MUST be included in `is_operational_command` in `src/lib.rs`.
- **Graph Maintenance:** Run `/home/jesse/.local/bin/graphify update .` after modifying code files.

## Review Focus

1. **Multi-byte UTF-8 in `stderr` truncation:** Truncating `stderr` to 1024 bytes when a multi-byte UTF-8 sequence straddles byte 1024 must not panic; slice at a valid char boundary (`floor_char_boundary` / `char_indices`).
2. **Shell metacharacter smuggling in LLM response:** An LLM returning `<command>grep foo src/; rm -rf /</command>` or `<command>grep foo $(whoami)</command>` must be rejected by `validate_healed_command` before spawning.
3. **Identical command returned by LLM:** If the LLM returns the exact same command that just failed, `validate_healed_command` must reject it as a no-op rather than wasting a second execution.
4. **Already-wrapped `rtk heal` idempotency:** If a command reaching `decide_with_params` is already `rtk heal -- ...`, the hook must not wrap it a second time.
5. **Empty or whitespace-only `<command>` tag:** `<command>   </command>` must be treated as a parse failure and trigger immediate fail-open.

---

### Task 1: Configuration & Environment Overrides (`HealConfig`)

**Files:**
- Modify: `src/core/config.rs`

**Interfaces:**
- Consumes: `crate::core::user_env::var`
- Produces:
  - `pub struct HealConfig { pub enabled: bool, pub endpoint: String, pub model: String, pub timeout_ms: u64 }` on `Config.heal`
  - `pub struct ResolvedHealConfig { pub enabled: bool, pub endpoint: String, pub model: String, pub api_key: Option<String>, pub timeout_ms: u64 }`
  - `impl HealConfig { pub fn resolve(&self) -> ResolvedHealConfig }`
  - `pub fn heal_enabled() -> bool` in `src/core/config.rs`

- [ ] **Step 1: Write the failing config and env resolution tests in `src/core/config.rs`**

```rust
#[test]
fn test_heal_config_defaults_and_env_overrides() {
    let cfg = Config::default();
    assert!(!cfg.heal.enabled);
    assert_eq!(cfg.heal.endpoint, "http://localhost:11434/v1");
    assert_eq!(cfg.heal.model, "qwen2.5-coder:1.5b");
    assert_eq!(cfg.heal.timeout_ms, 3000);

    crate::core::user_env::with_vars(
        &[
            ("RTK_HEAL_ENDPOINT", Some("http://127.0.0.1:9999/v1")),
            ("RTK_HEAL_MODEL", Some("claude-3-5-haiku-20241022")),
            ("RTK_HEAL_API_KEY", Some("sk-test-123")),
            ("RTK_HEAL_TIMEOUT_MS", Some("1500")),
        ],
        || {
            let resolved = cfg.heal.resolve();
            assert_eq!(resolved.endpoint, "http://127.0.0.1:9999/v1");
            assert_eq!(resolved.model, "claude-3-5-haiku-20241022");
            assert_eq!(resolved.api_key.as_deref(), Some("sk-test-123"));
            assert_eq!(resolved.timeout_ms, 1500);
        },
    );
}

#[test]
fn test_heal_config_toml_deserialization() {
    let toml_str = r#"
[heal]
enabled = true
endpoint = "http://localhost:8080/v1"
model = "llama3.2:3b"
timeout_ms = 2500
"#;
    let cfg: Config = toml::from_str(toml_str).expect("valid config toml");
    assert!(cfg.heal.enabled);
    assert_eq!(cfg.heal.endpoint, "http://localhost:8080/v1");
    assert_eq!(cfg.heal.model, "llama3.2:3b");
    assert_eq!(cfg.heal.timeout_ms, 2500);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib core::config::tests::test_heal_config`
Expected: FAIL with `no field heal on type Config`

- [ ] **Step 3: Implement `HealConfig`, `ResolvedHealConfig`, and `heal_enabled()` in `src/core/config.rs`**

Add `#[serde(default)] pub heal: HealConfig` to `Config`. Use string literals in `user_env::var("RTK_HEAL_ENDPOINT")`, `user_env::var("RTK_HEAL_MODEL")`, `user_env::var("RTK_HEAL_API_KEY")`, and `user_env::var("RTK_HEAL_TIMEOUT_MS")` so `test_isolation::every_variable_rtk_reads_is_redirected_for_a_child` resolves them statically. Read `cached_config().heal.enabled` in `pub fn heal_enabled() -> bool`.

- [ ] **Step 4: Run config and test_isolation tests to verify they pass**

Run: `cargo test --lib core::config::tests::test_heal_config && cargo test --lib core::test_isolation`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/core/config.rs
git commit -m "feat(config): add [heal] configuration and RTK_HEAL_* env resolution"
```

---

### Task 2: Safety Envelope & Command Sanitization (`src/cmds/system/heal_cmd/safety.rs`)

**Files:**
- Create: `src/cmds/system/heal_cmd/safety.rs`
- Create: `src/cmds/system/heal_cmd/mod.rs`
- Modify: `src/cmds/system/mod.rs`

**Interfaces:**
- Consumes: `crate::discover::lexer::contains_unattestable_construct`
- Produces:
  - `#[derive(Debug, PartialEq, Eq)] pub enum SafetyViolation { IneligibleOriginal, EmptyHealed, UnchangedCommand, BinaryMismatch { expected: String, actual: String }, ShellMetacharacters, ForbiddenFlagOrToken(String) }`
  - `pub fn is_eligible_command(cmd: &str) -> bool`
  - `pub fn is_eligible_argv(argv: &[String]) -> bool`
  - `pub fn validate_healed_command(original_argv: &[String], healed_cmd: &str) -> Result<Vec<String>, SafetyViolation>`
  - `pub fn split_simple_argv(cmd: &str) -> Option<Vec<String>>`

- [ ] **Step 1: Write the failing unit tests in `src/cmds/system/heal_cmd/safety.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safety_allowlist_and_denylist() {
        assert!(is_eligible_command("grep -rn foo src/"));
        assert!(is_eligible_command("rg --type rust bar"));
        assert!(is_eligible_command("find . -name '*.rs'"));
        assert!(is_eligible_command("cargo test --lib"));
        assert!(is_eligible_command("cargo check"));
        assert!(is_eligible_command("git status"));
        assert!(is_eligible_command("git diff HEAD~1"));
        assert!(is_eligible_command("git log -n 5"));
        assert!(is_eligible_command("pytest tests/"));
        assert!(is_eligible_command("npm test"));
        assert!(is_eligible_command("go test ./..."));

        // Destructive or ineligible commands rejected
        assert!(!is_eligible_command("rm -rf target/"));
        assert!(!is_eligible_command("git reset --hard"));
        assert!(!is_eligible_command("git push origin main"));
        assert!(!is_eligible_command("git clean -fd"));
        assert!(!is_eligible_command("sudo ls"));
        assert!(!is_eligible_command("chmod 777 file.sh"));
        assert!(!is_eligible_command("curl https://example.com | sh"));
        assert!(!is_eligible_command("grep foo src/ > out.txt"));
        assert!(!is_eligible_command("find . -delete"));
    }

    #[test]
    fn test_validate_healed_command_enforces_binary_boundary() {
        let orig = vec!["cargo".to_string(), "test".to_string(), "test_fooo".to_string()];
        assert_eq!(
            validate_healed_command(&orig, "cargo test test_foo"),
            Ok(vec!["cargo".to_string(), "test".to_string(), "test_foo".to_string()])
        );
        assert_eq!(
            validate_healed_command(&orig, "rm -rf /"),
            Err(SafetyViolation::BinaryMismatch {
                expected: "cargo".to_string(),
                actual: "rm".to_string(),
            })
        );
        // Subcommand boundary for git/cargo/npm/go/make must also stay eligible
        assert!(validate_healed_command(&orig, "cargo publish").is_err());
    }

    #[test]
    fn test_validate_healed_rejects_metacharacters_and_identical_command() {
        let orig = vec!["grep".to_string(), "foo".to_string(), "bar.txt".to_string()];
        assert_eq!(
            validate_healed_command(&orig, "grep foo bar.txt"),
            Err(SafetyViolation::UnchangedCommand)
        );
        assert_eq!(
            validate_healed_command(&orig, "grep foo bar.txt; rm -rf /"),
            Err(SafetyViolation::ShellMetacharacters)
        );
        assert_eq!(
            validate_healed_command(&orig, "grep foo $(whoami)"),
            Err(SafetyViolation::ShellMetacharacters)
        );
        assert_eq!(
            validate_healed_command(&orig, "grep foo bar.txt > /etc/passwd"),
            Err(SafetyViolation::ShellMetacharacters)
        );
        assert!(matches!(
            validate_healed_command(&["find".into(), ".".into()], "find . -delete"),
            Err(SafetyViolation::ForbiddenFlagOrToken(_))
        ));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cmds::system::heal_cmd::safety`
Expected: FAIL (module / functions not defined)

- [ ] **Step 3: Implement `safety.rs` and expose `pub mod heal_cmd;` in `src/cmds/system/mod.rs`**

Implement `split_simple_argv` (rejecting unquoted `;`, `|`, `&`, `>`, `<`, `` ` ``, `$()`, `\n`), `is_eligible_argv` / `is_eligible_command` checking single-command eligibility:
- Standalone read/search binaries: `grep`, `rg`, `find`, `cat`, `ls`, `file`, `stat`, `head`, `tail`
- Subcommand-scoped binaries:
  - `git` followed by `status`, `diff`, `log`, `show`, `branch` (without `-D` / `--delete` / `-f` / `--force`)
  - `cargo` followed by `test`, `check`
  - `pytest` (any non-destructive args)
  - `npm` followed by `test`
  - `go` followed by `test`
  - `make` followed by `test`
- Reject forbidden flags anywhere in argv: `--delete`, `-delete`, `--force`, `-f` (for `git` or `rm`/`mv`), `--hard`, `-exec`, `-execdir`, `-ok`, `--output`, `-o` (for `grep`/`rg`/`find`).

- [ ] **Step 4: Run safety unit tests to verify they pass**

Run: `cargo test --lib cmds::system::heal_cmd::safety`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/cmds/system/mod.rs src/cmds/system/heal_cmd/mod.rs src/cmds/system/heal_cmd/safety.rs
git commit -m "feat(heal): implement command eligibility allowlist and healed command sanitizer"
```

---

### Task 3: Prompt Builder, Tag Parser & Synchronous LLM Client (`src/cmds/system/heal_cmd/client.rs`)

**Files:**
- Create: `src/cmds/system/heal_cmd/client.rs`
- Modify: `src/cmds/system/heal_cmd/mod.rs`

**Interfaces:**
- Consumes: `crate::core::config::ResolvedHealConfig`
- Produces:
  - `pub fn truncate_stderr(stderr: &str) -> &str`
  - `pub fn extract_command_tag(response_text: &str) -> Option<String>`
  - `pub fn build_request_payload(model: &str, cwd: &str, original_cmd: &str, exit_code: i32, stderr: &str) -> serde_json::Value`
  - `pub fn request_healed_command(cfg: &ResolvedHealConfig, cwd: &str, original_cmd: &str, exit_code: i32, stderr: &str) -> Option<String>`

- [ ] **Step 1: Write the failing unit and mock-HTTP tests in `src/cmds/system/heal_cmd/client.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::ResolvedHealConfig;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_extract_command_tag_cases() {
        assert_eq!(
            extract_command_tag("<command>cargo test test_foo</command>"),
            Some("cargo test test_foo".to_string())
        );
        assert_eq!(
            extract_command_tag("Here is the fix:\n<command>  grep -rn foo src/  </command>\n"),
            Some("grep -rn foo src/".to_string())
        );
        assert_eq!(extract_command_tag("<command>   </command>"), None);
        assert_eq!(extract_command_tag("cargo test test_foo"), None);
        assert_eq!(
            extract_command_tag("<command>grep foo\nrm -rf /</command>"),
            None
        );
    }

    #[test]
    fn test_truncate_stderr_respects_lines_bytes_and_utf8_boundaries() {
        let many_lines = (1..=60).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let truncated = truncate_stderr(&many_lines);
        assert_eq!(truncated.lines().count(), 40);
        assert!(truncated.starts_with("line 21"));

        // Multi-byte UTF-8 ("é" is 2 bytes, "🦀" is 4 bytes) exceeding 1024 bytes
        let utf8_heavy = "🦀".repeat(400); // 1600 bytes
        let truncated_utf8 = truncate_stderr(&utf8_heavy);
        assert!(truncated_utf8.len() <= 1024);
        assert_eq!(truncated_utf8.len() % 4, 0);
    }

    #[test]
    fn test_request_healed_command_success_and_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let body = r#"{"choices":[{"message":{"content":"<command>cargo test test_foo</command>"}}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let cfg = ResolvedHealConfig {
            enabled: true,
            endpoint: format!("http://{addr}/v1"),
            model: "qwen2.5-coder:1.5b".to_string(),
            api_key: None,
            timeout_ms: 1000,
        };
        let healed = request_healed_command(&cfg, "/repo", "cargo test test_fooo", 101, "no test target");
        assert_eq!(healed.as_deref(), Some("cargo test test_foo"));

        // Slow server triggers timeout fail-open
        let slow_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let slow_addr = slow_listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((_stream, _)) = slow_listener.accept() {
                thread::sleep(Duration::from_millis(500));
            }
        });
        let slow_cfg = ResolvedHealConfig {
            timeout_ms: 100,
            endpoint: format!("http://{slow_addr}/v1"),
            ..cfg
        };
        assert_eq!(
            request_healed_command(&slow_cfg, "/repo", "cargo test test_fooo", 101, "err"),
            None
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cmds::system::heal_cmd::client`
Expected: FAIL (module / functions not defined)

- [ ] **Step 3: Implement `src/cmds/system/heal_cmd/client.rs`**

Implement:
- `truncate_stderr(stderr: &str) -> &str`: takes the tail of at most 40 lines and at most 1024 bytes, advancing the start index to the nearest UTF-8 char boundary (`!stderr.is_char_boundary(idx)`).
- `extract_command_tag(response_text: &str) -> Option<String>`: extracts the substring between `<command>` and `</command>`, trims whitespace, strips optional surrounding backticks, and returns `None` if empty or if it contains `\n` or `\r`.
- `build_request_payload`: formats the exact System and User messages specified in Section 3.3 of the spec with `temperature: 0.0` and `max_tokens: 128`.
- `request_healed_command`: builds a `ureq::AgentBuilder::new().timeout(Duration::from_millis(cfg.timeout_ms)).build()`, POSTs to `{endpoint}/chat/completions` with optional `Authorization: Bearer {api_key}`, parses `/choices/0/message/content`, and calls `extract_command_tag`.

- [ ] **Step 4: Run client unit tests to verify they pass**

Run: `cargo test --lib cmds::system::heal_cmd::client`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/cmds/system/heal_cmd/client.rs src/cmds/system/heal_cmd/mod.rs
git commit -m "feat(heal): add prompt builder, tag parser, and ureq LLM client"
```

---

### Task 4: `rtk heal` Execution Runner & CLI Subcommand Wiring

**Files:**
- Modify: `src/cmds/system/heal_cmd/mod.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes:
  - `crate::core::config::{Config, ResolvedHealConfig}`
  - `crate::core::shell::{Launch, command_from_args, display_command, program_name, spawn_failure}`
  - `crate::core::user_dirs`
  - `safety::{is_eligible_argv, validate_healed_command}`
  - `client::request_healed_command`
- Produces:
  - `pub struct HealOutcome { pub stdout: String, pub stderr: String, pub exit_code: i32, pub healed_to: Option<String> }`
  - `pub fn execute_with_config(command: &[String], cfg: &ResolvedHealConfig) -> Result<HealOutcome>`
  - `pub fn run(command: &[String], timeout_ms: Option<u64>, model: Option<&str>, verbose: u8) -> Result<i32>`
  - `Commands::Heal { timeout_ms: Option<u64>, model: Option<String>, command: Vec<String> }` in `src/lib.rs`

- [ ] **Step 1: Write the failing execution runner and CLI integration tests in `src/cmds/system/heal_cmd/mod.rs` and `src/lib.rs`**

In `src/cmds/system/heal_cmd/mod.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::ResolvedHealConfig;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn test_pass_through_on_zero_makes_no_http_calls() {
        let cfg = ResolvedHealConfig {
            enabled: true,
            endpoint: "http://127.0.0.1:1".to_string(), // unreachable port
            model: "test".to_string(),
            api_key: None,
            timeout_ms: 100,
        };
        let cmd = vec!["git".to_string(), "status".to_string()];
        let out = execute_with_config(&cmd, &cfg).expect("execution succeeds");
        assert_eq!(out.exit_code, 0);
        assert!(out.healed_to.is_none());
    }

    #[test]
    fn test_heal_success_prepends_banner_and_exits_zero() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let body = r#"{"choices":[{"message":{"content":"<command>git status</command>"}}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let cfg = ResolvedHealConfig {
            enabled: true,
            endpoint: format!("http://{addr}/v1"),
            model: "test".to_string(),
            api_key: None,
            timeout_ms: 1000,
        };
        let cmd = vec!["git".to_string(), "status".to_string(), "--not-a-real-flag-xyz".to_string()];
        let out = execute_with_config(&cmd, &cfg).expect("execution succeeds");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.healed_to.as_deref(), Some("git status"));
        assert!(out.stdout.starts_with("[rtk: auto-corrected to `git status`]\n"));
    }

    #[test]
    fn test_heal_fail_open_returns_original_stderr_and_code() {
        let cfg = ResolvedHealConfig {
            enabled: true,
            endpoint: "http://127.0.0.1:1".to_string(), // unreachable -> fail open
            model: "test".to_string(),
            api_key: None,
            timeout_ms: 50,
        };
        let cmd = vec!["git".to_string(), "status".to_string(), "--not-a-real-flag-xyz".to_string()];
        let out = execute_with_config(&cmd, &cfg).expect("execution succeeds");
        assert_ne!(out.exit_code, 0);
        assert!(out.healed_to.is_none());
        assert!(out.stderr.contains("not-a-real-flag-xyz"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cmds::system::heal_cmd::tests`
Expected: FAIL (`execute_with_config` not defined)

- [ ] **Step 3: Implement `execute_with_config` and `run` in `src/cmds/system/heal_cmd/mod.rs` and wire `Commands::Heal` in `src/lib.rs`**

In `src/cmds/system/heal_cmd/mod.rs`:
1. Run `command` via `command_from_args(command, None)` and `.output()`.
2. If exit code is 0, return `HealOutcome` with `exit_code: 0` and `healed_to: None`.
3. If exit code != 0:
   - Check `safety::is_eligible_argv(command)`. If false, return original `HealOutcome`.
   - Resolve `cwd` via `crate::core::user_dirs::current_dir()`.
   - Call `client::request_healed_command(cfg, &cwd_str, &original_display, orig_code, &orig_stderr)`.
   - Validate via `safety::validate_healed_command(command, &healed_str)`.
   - Run healed argv via `command_from_args(&healed_argv, None)`.
   - If healed exit code == 0, format `stdout` with `[rtk: auto-corrected to \`{healed_str}\`]\n{healed_stdout}` and return `exit_code: 0`.
   - Otherwise, return the **original** `stdout`, `stderr`, and `exit_code`.

In `src/lib.rs`:
- Add `use cmds::system::heal_cmd;`
- Add `Commands::Heal { #[arg(long)] timeout_ms: Option<u64>, #[arg(long)] model: Option<String>, #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)] command: Vec<String> }`
- Add `Commands::Heal { .. }` to `is_operational_command` and add `&["heal", "--", "git", "status"]` to `test_hook_rewritten_commands_get_the_hook_integrity_check`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib cmds::system::heal_cmd && cargo test --lib tests::test_hook_rewritten_commands_get_the_hook_integrity_check`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/cmds/system/heal_cmd/mod.rs src/lib.rs
git commit -m "feat(heal): implement rtk heal runner and CLI subcommand"
```

---

### Task 5: `PreToolUse` Hook Integration (`src/hooks/decision.rs`)

**Files:**
- Modify: `src/hooks/decision.rs`

**Interfaces:**
- Consumes:
  - `crate::core::config::heal_enabled`
  - `crate::cmds::system::heal_cmd::safety::is_eligible_command`
- Produces:
  - `pub(crate) fn decide_with_heal(cmd: &str, verdict: PermissionVerdict, excluded: &[String], transparent_prefixes: &[String], heal: bool) -> HookDecision`

- [ ] **Step 1: Write the failing hook decision tests in `src/hooks/decision.rs`**

```rust
#[test]
fn test_decide_with_heal_wraps_eligible_commands_when_enabled() {
    assert_eq!(
        decide_with_heal("cargo test test_foo", PermissionVerdict::Allow, &[], &[], true),
        HookDecision::AllowRewrite("rtk heal -- cargo test test_foo".to_string())
    );
    assert_eq!(
        decide_with_heal("grep -rn pattern src/", PermissionVerdict::Default, &[], &[], true),
        HookDecision::AskRewrite("rtk heal -- grep -rn pattern src/".to_string())
    );
}

#[test]
fn test_decide_with_heal_skips_ineligible_denied_or_already_wrapped() {
    // Denied command stays Deny
    assert_eq!(
        decide_with_heal("cargo test", PermissionVerdict::Deny, &[], &[], true),
        HookDecision::Deny
    );
    // Destructive command is not wrapped with heal
    assert_eq!(
        decide_with_heal("rm -rf target", PermissionVerdict::Allow, &[], &[], true),
        HookDecision::Defer
    );
    // Already wrapped with rtk heal is not double-wrapped
    assert_eq!(
        decide_with_heal("rtk heal -- cargo test", PermissionVerdict::Allow, &[], &[], true),
        HookDecision::AllowRewrite("rtk heal -- cargo test".to_string())
    );
    // When heal = false, standard rtk rewrite behavior is preserved
    assert_eq!(
        decide_with_heal("cargo test", PermissionVerdict::Allow, &[], &[], false),
        HookDecision::AllowRewrite("rtk cargo test".to_string())
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib hooks::decision::tests::test_decide_with_heal`
Expected: FAIL (`decide_with_heal` not defined)

- [ ] **Step 3: Implement `decide_with_heal` in `src/hooks/decision.rs`**

In `src/hooks/decision.rs`:
- Update `decide(cmd: &str, verdict: PermissionVerdict) -> HookDecision` to read `let heal = crate::core::config::heal_enabled();` and delegate to `decide_with_heal(cmd, verdict, &excluded, &transparent_prefixes, heal)`.
- Keep `decide_with_params` calling `decide_with_heal(cmd, verdict, excluded, transparent_prefixes, false)` so all existing callers and unit tests of `decide_with_params` remain 100% unchanged.
- In `decide_with_heal`:
  - After `Deny`, `Defer`, and `contains_unattestable_construct` checks:
  - If `cmd.trim_start().starts_with("rtk heal ")`: return `AllowRewrite` (if `Allow`) or `AskRewrite` unchanged so `suppress_identity` treats it as an identity rewrite.
  - If `heal && !excluded_matches && crate::cmds::system::heal_cmd::safety::is_eligible_command(cmd)`: return `AllowRewrite(format!("rtk heal -- {}", cmd.trim()))` on `PermissionVerdict::Allow`, else `AskRewrite(format!("rtk heal -- {}", cmd.trim()))`.
  - Otherwise fall through to `decide_with_params`.

- [ ] **Step 4: Run all hook decision and test_isolation tests to verify they pass**

Run: `cargo test --lib hooks::decision && cargo test --lib core::test_isolation`
Expected: PASS

- [ ] **Step 5: Update knowledge graph and commit**

```bash
/home/jesse/.local/bin/graphify update .
git add src/hooks/decision.rs graphify-out/
git commit -m "feat(hooks): wire PreToolUse auto-healing rewrite when heal.enabled is true"
```
