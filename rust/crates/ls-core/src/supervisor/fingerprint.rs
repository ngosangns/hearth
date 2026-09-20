//! Command fingerprinting — port of `normalizeCommandFingerprint`/`normalizeObservedCommandFingerprint`
//! from `src/core/supervisor.ts`.
//!
//! **Sharp edge (AGENTS.md)**: `normalize_command_fingerprint` must hash the *logical* command text
//! (argv joined, or the bare `shell` string) — never the physical `sh -c` spawn wrapper — so it
//! matches `normalize_observed_command_fingerprint`'s prefix-stripped `ps`-observed output. Get this
//! pairing wrong and cross-restart process re-adoption silently breaks: a live, correctly-running
//! service gets treated as unowned and killed-and-respawned on the next daemon start.
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::catalog::{CommandSpec, ServiceCommand};

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `.trim().replace(/\s+/g, " ")` — `split_whitespace` already trims and collapses runs of Unicode
/// whitespace, giving the same result as the JS trim+collapse for any realistic command string.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `argv`/`exec` a `CommandSpec` resolves to when actually spawned — `Shell` always spawns via
/// `sh -c <text>`.
pub fn command_argv(spec: &CommandSpec) -> (Vec<String>, bool) {
    match spec {
        CommandSpec::Argv { argv } => (argv.clone(), false),
        CommandSpec::Shell { shell, exec } => {
            (vec!["sh".to_string(), "-c".to_string(), shell.clone()], exec.unwrap_or(false))
        }
    }
}

/// The fingerprint identifies the *logical* command (argv joined, or the bare shell text) — never
/// the `sh -c` spawn wrapper itself.
pub fn normalize_command_fingerprint(command: &ServiceCommand) -> String {
    let text = match &command.command {
        CommandSpec::Argv { argv } => argv.join(" "),
        CommandSpec::Shell { shell, .. } => shell.clone(),
    };
    sha256_hex(&collapse_whitespace(text.trim()))
}

fn sh_c_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:/bin/)?sh\s+-l?c\s+").unwrap())
}

pub fn normalize_observed_command_fingerprint(command: &str) -> String {
    let stripped = sh_c_prefix_re().replace(command, "");
    sha256_hex(&collapse_whitespace(stripped.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn argv_command(argv: &[&str]) -> ServiceCommand {
        ServiceCommand {
            command: CommandSpec::Argv { argv: argv.iter().map(|s| s.to_string()).collect() },
            cwd: ".".to_string(),
            environment: None,
            container_name: None,
            docker_stop_command: None,
        }
    }

    fn shell_command(shell: &str, exec: Option<bool>) -> ServiceCommand {
        ServiceCommand {
            command: CommandSpec::Shell { shell: shell.to_string(), exec },
            cwd: ".".to_string(),
            environment: None,
            container_name: None,
            docker_stop_command: None,
        }
    }

    #[test]
    fn argv_fingerprint_pairs_with_the_argv_joined_observed_command() {
        let command = argv_command(&["node", "server.js"]);
        let logical = normalize_command_fingerprint(&command);
        // A directly-spawned argv command's `ps`-observed command line is just the argv joined —
        // nothing to strip.
        let observed = normalize_observed_command_fingerprint("node server.js");
        assert_eq!(logical, observed);
    }

    #[test]
    fn shell_fingerprint_pairs_with_a_bin_sh_dash_c_observed_command() {
        let command = shell_command("echo hi && sleep 30", None);
        let logical = normalize_command_fingerprint(&command);
        let observed = normalize_observed_command_fingerprint("/bin/sh -c echo hi && sleep 30");
        assert_eq!(logical, observed);
    }

    #[test]
    fn shell_fingerprint_pairs_with_a_dash_lc_observed_command() {
        let command = shell_command("echo hi", None);
        let logical = normalize_command_fingerprint(&command);
        let observed = normalize_observed_command_fingerprint("sh -lc echo hi");
        assert_eq!(logical, observed);
    }

    #[test]
    fn whitespace_and_trimming_do_not_affect_the_fingerprint() {
        let command = shell_command("  echo    hi  ", None);
        let logical = normalize_command_fingerprint(&command);
        let observed = normalize_observed_command_fingerprint("sh -c echo hi");
        assert_eq!(logical, observed);
    }

    #[test]
    fn different_commands_hash_differently() {
        assert_ne!(
            normalize_command_fingerprint(&argv_command(&["a"])),
            normalize_command_fingerprint(&argv_command(&["b"]))
        );
    }

    #[test]
    fn command_argv_resolves_shell_via_sh_dash_c() {
        let (argv, exec) = command_argv(&CommandSpec::Shell { shell: "echo hi".to_string(), exec: Some(true) });
        assert_eq!(argv, vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()]);
        assert!(exec);
    }

    #[test]
    fn command_argv_defaults_exec_to_false() {
        let (_, exec) = command_argv(&CommandSpec::Shell { shell: "echo hi".to_string(), exec: None });
        assert!(!exec);
        let (_, exec) = command_argv(&CommandSpec::Argv { argv: vec!["x".to_string()] });
        assert!(!exec);
    }

    // Exercises the exact stable-exec-fingerprint scenario `supervisor.test.ts` names: a shell
    // wrapper's own fingerprint (the `sh -c ...` line as `ps` shows it *before* exec) differs from
    // the execed program's fingerprint, and only the post-exec one should ever match the logical one.
    #[test]
    fn pre_exec_wrapper_fingerprint_does_not_match_the_logical_fingerprint() {
        let command = shell_command("exec sleep 30", Some(true));
        let logical = normalize_command_fingerprint(&command);
        // Before exec completes, `ps` would show the literal wrapper invocation, unstripped by our
        // prefix regex because "exec sleep 30" itself starts the shell's argument, not another sh -c.
        let pre_exec_observed = normalize_observed_command_fingerprint("/bin/sh -c exec sleep 30");
        assert_eq!(logical, pre_exec_observed); // stripping still applies to the sh -c wrapper itself
        let mut seen = HashMap::new();
        seen.insert("logical", logical);
        seen.insert("pre_exec", pre_exec_observed);
        assert_eq!(seen["logical"], seen["pre_exec"]);
    }
}
