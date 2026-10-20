pub mod client;
pub mod safety;

use crate::core::config::{Config, ResolvedHealConfig};
use crate::core::shell::{Launch, command_from_args, display_command};
use crate::core::user_dirs;
use anyhow::{Context, Result};
use std::io::{self, Write};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub healed_to: Option<String>,
}

fn run_command_raw(command: &[String]) -> Result<(String, String, i32, bool)> {
    match command_from_args(command, None).context("Failed to prepare command")? {
        Launch::Ready(mut cmd) => {
            let output = cmd.output().context("Failed to execute command")?;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let is_signal = output.status.code().is_none();
            let exit_code = crate::core::utils::exit_code_from_output(&output, "heal");
            Ok((stdout, stderr, exit_code, is_signal))
        }
        Launch::Unrunnable(outcome) => Ok((String::new(), outcome.message, outcome.code, false)),
    }
}

pub fn execute_with_config(command: &[String], cfg: &ResolvedHealConfig) -> Result<HealOutcome> {
    let (stdout, stderr, exit_code, is_signal) = run_command_raw(command)?;
    if exit_code == 0 || is_signal {
        return Ok(HealOutcome {
            stdout,
            stderr,
            exit_code,
            healed_to: None,
        });
    }

    // Check if command is eligible for healing
    if !safety::is_eligible_argv(command) {
        return Ok(HealOutcome {
            stdout,
            stderr,
            exit_code,
            healed_to: None,
        });
    }

    // If stderr is empty (e.g. grep found no matches with exit 1), there is no error diagnosis to heal.
    if stderr.trim().is_empty() {
        return Ok(HealOutcome {
            stdout,
            stderr,
            exit_code,
            healed_to: None,
        });
    }

    let cwd = user_dirs::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let orig_display = display_command(command);

    if let Some(healed_str) =
        client::request_healed_command(cfg, &cwd, &orig_display, exit_code, &stderr)
    {
        if let Ok(healed_argv) = safety::validate_healed_command(command, &healed_str) {
            if let Ok((healed_out, healed_err, healed_code, _)) = run_command_raw(&healed_argv) {
                if healed_code == 0 {
                    let banner = format!("[rtk: auto-corrected to `{healed_str}`]\n");
                    return Ok(HealOutcome {
                        stdout: format!("{banner}{healed_out}"),
                        stderr: healed_err,
                        exit_code: 0,
                        healed_to: Some(healed_str),
                    });
                }
            }
        }
    }

    // Fail open: return the original outcome
    Ok(HealOutcome {
        stdout,
        stderr,
        exit_code,
        healed_to: None,
    })
}

pub fn run(
    command: &[String],
    timeout_ms: Option<u64>,
    model: Option<&str>,
    _verbose: u8,
) -> Result<i32> {
    let base_cfg = Config::load()
        .map(|c| c.heal.resolve())
        .unwrap_or_else(|_| Config::default().heal.resolve());

    let cfg = ResolvedHealConfig {
        enabled: true,
        endpoint: base_cfg.endpoint,
        model: model.map(|m| m.to_string()).unwrap_or(base_cfg.model),
        api_key: base_cfg.api_key,
        timeout_ms: timeout_ms.unwrap_or(base_cfg.timeout_ms),
    };

    let outcome = execute_with_config(command, &cfg)?;
    if !outcome.stdout.is_empty() {
        let _ = io::stdout().write_all(outcome.stdout.as_bytes());
    }
    if !outcome.stderr.is_empty() {
        let _ = io::stderr().write_all(outcome.stderr.as_bytes());
    }

    Ok(outcome.exit_code)
}

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

    #[test]
    fn test_heal_empty_stderr_fails_open_without_calling_llm() {
        let cfg = ResolvedHealConfig {
            enabled: true,
            endpoint: "http://127.0.0.1:1".to_string(), // unreachable port
            model: "test".to_string(),
            api_key: None,
            timeout_ms: 5000,
        };
        let cmd = vec![
            "grep".to_string(),
            "this_string_definitely_does_not_exist_xyz_123".to_string(),
            "Cargo.toml".to_string(),
        ];
        let out = execute_with_config(&cmd, &cfg).expect("execution succeeds");
        assert_eq!(out.exit_code, 1);
        assert!(out.healed_to.is_none());
    }
}
