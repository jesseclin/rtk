//! Command eligibility and safety validation for auto-healing.

use crate::discover::lexer::{TokenKind, contains_unattestable_construct, strip_quotes, tokenize};
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub enum SafetyViolation {
    IneligibleOriginal,
    EmptyHealed,
    UnchangedCommand,
    BinaryMismatch { expected: String, actual: String },
    ShellMetacharacters,
    ForbiddenFlagOrToken(String),
}

/// Standalone safe inspection, search, and reading utilities.
const SAFE_STANDALONE_BINARIES: &[&str] = &[
    "grep", "rg", "find", "cat", "ls", "file", "stat", "head", "tail",
];

/// Tokens, flags, or subcommands that are strictly forbidden anywhere in argv.
const FORBIDDEN_TOKENS: &[&str] = &[
    "--delete", "-delete", "--hard", "-exec", "-execdir", "-ok", "-okdir",
    "-fls", "-fprint", "-fprint0", "-fprintf",
];

/// Split a command into simple arguments, refusing shell operators or metacharacters.
pub fn split_simple_argv(cmd: &str) -> Option<Vec<String>> {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return None;
    }

    if contains_unattestable_construct(trimmed) {
        return None;
    }

    // Additional check for shell operators that might not be in unattestable construct
    if trimmed.contains(';')
        || trimmed.contains('&')
        || trimmed.contains('|')
        || trimmed.contains('>')
        || trimmed.contains('<')
        || trimmed.contains('`')
        || trimmed.contains('\n')
        || trimmed.contains('\r')
    {
        return None;
    }

    let tokens = tokenize(trimmed);
    if tokens.is_empty() {
        return None;
    }

    let mut argv = Vec::with_capacity(tokens.len());
    for t in tokens {
        if t.kind != TokenKind::Arg {
            return None;
        }
        argv.push(strip_quotes(&t.value));
    }

    Some(argv)
}

/// Check if a command is eligible for auto-healing.
pub fn is_eligible_command(cmd: &str) -> bool {
    let Some(argv) = split_simple_argv(cmd) else {
        return false;
    };
    is_eligible_argv(&argv)
}

/// Check if an argv vector is eligible for auto-healing.
pub fn is_eligible_argv(argv: &[String]) -> bool {
    if argv.is_empty() {
        return false;
    }

    let raw_bin = &argv[0];
    let bin = Path::new(raw_bin)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(raw_bin);

    // Scan for forbidden tokens / flags anywhere in argv
    for arg in argv {
        if FORBIDDEN_TOKENS.contains(&arg.as_str()) {
            return false;
        }
        if arg.starts_with('>') || arg.starts_with('<') {
            return false;
        }
        if arg.starts_with("--output") || arg == "-o" {
            return false;
        }
    }

    // 1. Standalone inspection/search binaries
    if SAFE_STANDALONE_BINARIES.contains(&bin) {
        return true;
    }

    // 2. Subcommand-scoped binaries
    match bin {
        "git" => {
            let Some(subcmd) = argv.get(1) else {
                return false;
            };
            match subcmd.as_str() {
                "status" | "diff" | "log" | "show" => true,
                "branch" => {
                    // git branch is safe only without delete flags
                    !argv.iter().any(|a| a == "-d" || a == "-D" || a == "--delete")
                }
                _ => false,
            }
        }
        "cargo" => {
            let Some(subcmd) = argv.get(1) else {
                return false;
            };
            matches!(subcmd.as_str(), "test" | "check")
        }
        "pytest" => true,
        "npm" => {
            let Some(subcmd) = argv.get(1) else {
                return false;
            };
            matches!(subcmd.as_str(), "test" | "t")
        }
        "go" => {
            let Some(subcmd) = argv.get(1) else {
                return false;
            };
            subcmd == "test"
        }
        "make" => argv.iter().skip(1).any(|a| a == "test" || a == "check"),
        _ => false,
    }
}

/// Validate that a healed command candidate meets safety invariants relative to the original.
pub fn validate_healed_command(
    original_argv: &[String],
    healed_cmd: &str,
) -> Result<Vec<String>, SafetyViolation> {
    if !is_eligible_argv(original_argv) {
        return Err(SafetyViolation::IneligibleOriginal);
    }

    let trimmed = healed_cmd.trim();
    if trimmed.is_empty() {
        return Err(SafetyViolation::EmptyHealed);
    }

    let Some(healed_argv) = split_simple_argv(trimmed) else {
        return Err(SafetyViolation::ShellMetacharacters);
    };

    if healed_argv == original_argv {
        return Err(SafetyViolation::UnchangedCommand);
    }

    let orig_raw_bin = &original_argv[0];
    let healed_raw_bin = &healed_argv[0];

    // Enforce exact binary invocation path match to prevent path spoofing (/tmp/cat vs cat)
    if orig_raw_bin != healed_raw_bin {
        return Err(SafetyViolation::BinaryMismatch {
            expected: orig_raw_bin.to_string(),
            actual: healed_raw_bin.to_string(),
        });
    }

    // For subcommand-scoped tools, the healed subcommand must match the original subcommand
    let bin = Path::new(orig_raw_bin)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(orig_raw_bin);

    match bin {
        "git" | "cargo" | "npm" | "go" => {
            if original_argv.get(1) != healed_argv.get(1) {
                return Err(SafetyViolation::ForbiddenFlagOrToken(
                    "Subcommand drift not allowed".to_string(),
                ));
            }
        }
        _ => {}
    }

    // Check for forbidden tokens
    for arg in &healed_argv {
        if FORBIDDEN_TOKENS.contains(&arg.as_str())
            || arg.starts_with("--output")
            || arg == "-o"
            || arg.starts_with('>')
            || arg.starts_with('<')
        {
            return Err(SafetyViolation::ForbiddenFlagOrToken(arg.clone()));
        }
    }

    if !is_eligible_argv(&healed_argv) {
        return Err(SafetyViolation::ForbiddenFlagOrToken(
            "Healed command is not an eligible safe command".to_string(),
        ));
    }

    Ok(healed_argv)
}

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

    #[test]
    fn test_validate_healed_rejects_binary_path_spoofing() {
        let orig = vec!["cat".to_string(), "file.txt".to_string()];
        assert_eq!(
            validate_healed_command(&orig, "/tmp/cat file.txt"),
            Err(SafetyViolation::BinaryMismatch {
                expected: "cat".to_string(),
                actual: "/tmp/cat".to_string(),
            })
        );
        assert_eq!(
            validate_healed_command(&orig, "./cat file.txt"),
            Err(SafetyViolation::BinaryMismatch {
                expected: "cat".to_string(),
                actual: "./cat".to_string(),
            })
        );
    }

    #[test]
    fn test_validate_healed_rejects_output_redirection_flags() {
        let orig_diff = vec!["git".to_string(), "diff".to_string()];
        assert!(validate_healed_command(&orig_diff, "git diff --output=/etc/passwd").is_err());
        assert!(validate_healed_command(&orig_diff, "git diff --output /etc/passwd").is_err());

        let orig_find = vec!["find".to_string(), ".".to_string()];
        assert!(validate_healed_command(&orig_find, "find . -fls /tmp/out").is_err());
        assert!(validate_healed_command(&orig_find, "find . -fprint /tmp/out").is_err());
    }

    #[test]
    fn test_validate_healed_rejects_subcommand_drift() {
        let orig_test = vec!["cargo".to_string(), "test".to_string(), "test_fooo".to_string()];
        assert!(matches!(
            validate_healed_command(&orig_test, "cargo check"),
            Err(SafetyViolation::ForbiddenFlagOrToken(_))
        ));

        let orig_status = vec!["git".to_string(), "status".to_string()];
        assert!(matches!(
            validate_healed_command(&orig_status, "git diff"),
            Err(SafetyViolation::ForbiddenFlagOrToken(_))
        ));
    }
}
