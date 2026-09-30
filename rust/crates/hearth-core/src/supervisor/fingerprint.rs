//! Command fingerprinting — the identity hash a managed process is re-recognized by across restarts.
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

/// The raw `ps` command line is still the `sh -c` wrapper, not the program it will exec.
/// Octal escapes (`\012` for a newline inside the script) keep the hash from matching the logical
/// command, so a hash compare alone will not recognize the wrapper.
pub fn is_shell_wrapper_command(command: &str) -> bool {
    sh_c_prefix_re().is_match(command.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_ps_wrapper_line_is_still_the_shell_even_with_an_octal_newline() {
        assert!(is_shell_wrapper_command("/bin/sh -c sleep 2\\012exec nginx"));
        assert!(is_shell_wrapper_command("sh -lc echo hi"));
        assert!(!is_shell_wrapper_command("nginx: master process nginx"));
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

    // A shell wrapper observed before exec completes (`/bin/sh -c exec sleep 30` in `ps`) normalizes
    // to the same fingerprint as the logical command: the `sh -c` prefix-stripping applies to the
    // wrapper line itself, so it is not mistaken for a foreign process.
    #[test]
    fn pre_exec_wrapper_fingerprint_matches_the_logical_fingerprint() {
        let command = shell_command("exec sleep 30", Some(true));
        let logical = normalize_command_fingerprint(&command);
        let pre_exec_observed = normalize_observed_command_fingerprint("/bin/sh -c exec sleep 30");
        assert_eq!(logical, pre_exec_observed);
    }
}
