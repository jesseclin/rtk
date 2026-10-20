//! Prompt building, tag parsing, and synchronous HTTP LLM client for command repair.

use crate::core::config::ResolvedHealConfig;
use std::time::Duration;

/// Truncate stderr to at most the last 40 lines and at most 1024 bytes,
/// slicing at a valid UTF-8 character boundary.
pub fn truncate_stderr(stderr: &str) -> &str {
    let total_bytes = stderr.len();
    if total_bytes == 0 {
        return "";
    }

    // 1. Line truncation: keep at most last 40 lines
    let mut line_start = 0;
    let mut count = 0;
    for (idx, b) in stderr.bytes().enumerate().rev() {
        if b == b'\n' {
            count += 1;
            if count == 40 {
                line_start = idx + 1;
                break;
            }
        }
    }

    // 2. Byte truncation: keep at most 1024 bytes from the end
    let byte_limit_start = if total_bytes > 1024 {
        total_bytes - 1024
    } else {
        0
    };

    let mut start = line_start.max(byte_limit_start);
    // Advance to the next valid char boundary if needed
    while start < total_bytes && !stderr.is_char_boundary(start) {
        start += 1;
    }

    &stderr[start..]
}

/// Extract command string enclosed in `<command>...</command>` tags.
pub fn extract_command_tag(response_text: &str) -> Option<String> {
    let start_tag = "<command>";
    let end_tag = "</command>";

    let start_idx = response_text.find(start_tag)? + start_tag.len();
    let end_idx = response_text[start_idx..].find(end_tag)? + start_idx;

    let cmd = response_text[start_idx..end_idx].trim();
    if cmd.is_empty() || cmd.contains('\n') || cmd.contains('\r') {
        return None;
    }

    // Strip optional surrounding backticks if the model enclosed it in `...`
    let unquoted = if cmd.starts_with('`') && cmd.ends_with('`') && cmd.len() >= 2 {
        cmd[1..cmd.len() - 1].trim()
    } else {
        cmd
    };

    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
}

/// Format the OpenAI-compatible JSON payload for the repair completion request.
pub fn build_request_payload(
    model: &str,
    cwd: &str,
    original_cmd: &str,
    exit_code: i32,
    stderr: &str,
) -> serde_json::Value {
    let truncated = truncate_stderr(stderr);
    let system_prompt = "You are a deterministic CLI repair assistant. Fix the failed terminal command.\n\
                         Return ONLY the corrected command enclosed in <command>...</command> tags.\n\
                         Do not explain. Do not include markdown code blocks.";
    let user_prompt = format!(
        "Directory: {cwd}\nOriginal Command: {original_cmd}\nExit Code: {exit_code}\nError Output:\n{truncated}"
    );

    serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": user_prompt }
        ],
        "temperature": 0.0,
        "max_tokens": 128
    })
}

/// Synchronously request a repaired command from the configured LLM endpoint.
///
/// Returns `None` on any transport failure, timeout, non-200 status, or invalid response format.
pub fn request_healed_command(
    cfg: &ResolvedHealConfig,
    cwd: &str,
    original_cmd: &str,
    exit_code: i32,
    stderr: &str,
) -> Option<String> {
    let url = format!("{}/chat/completions", cfg.endpoint.trim_end_matches('/'));
    let payload = build_request_payload(&cfg.model, cwd, original_cmd, exit_code, stderr);
    let body_bytes = serde_json::to_vec(&payload).ok()?;

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(cfg.timeout_ms))
        .build();

    let mut req = agent.post(&url).set("Content-Type", "application/json");
    if let Some(key) = &cfg.api_key {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }

    let resp = req.send_bytes(&body_bytes).ok()?;
    if resp.status() != 200 {
        return None;
    }

    let json: serde_json::Value = serde_json::from_reader(resp.into_reader()).ok()?;
    let content = json
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())?;

    extract_command_tag(content)
}

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
