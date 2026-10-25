//! Usage errors of the binary: a subcommand placed after load-test flags is
//! reported as a misplaced subcommand, not as an invalid URL, and
//! `wsblast <URL> [flags]` keeps working when the URL comes after flags.

use std::process::{Command, Output};

fn wsblast(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Subcommands every build has, plus the ones its features add.
fn subcommands() -> Vec<&'static str> {
    let mut names = vec!["render", "man", "completions"];
    if cfg!(feature = "mock") {
        names.push("mock");
    }
    if cfg!(feature = "distributed") {
        names.push("agent");
    }
    names
}

#[test]
fn a_subcommand_after_flags_is_reported_as_misplaced() {
    for name in subcommands() {
        let output = wsblast(&["-c", "2", name]);
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{name}: {stderr}");
        assert!(
            stderr.contains(&format!("'{name}' is a subcommand, not a target URL")),
            "{name}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("wsblast {name} [OPTIONS]")),
            "{name}: {stderr}"
        );
        assert!(!stderr.contains("Invalid target URL"), "{name}: {stderr}");
    }
}

#[test]
fn a_misplaced_subcommand_with_its_own_flags_gets_a_note() {
    let cases: Vec<&[&str]> = {
        let mut cases: Vec<&[&str]> = vec![&["--format", "json", "render", "report.json"]];
        if cfg!(feature = "mock") {
            cases.push(&["-c", "2", "mock", "--delay", "20ms"]);
        }
        if cfg!(feature = "distributed") {
            cases.push(&["-c", "2", "agent", "--listen", "0.0.0.0:7700"]);
        }
        cases
    };
    for args in cases {
        let output = wsblast(args);
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(
            stderr.contains("error: "),
            "clap's message: {args:?}: {stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "if '{}' is meant as a subcommand, it must come first",
                args[2]
            )),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn a_url_after_flags_is_still_a_target() {
    // Nothing listens on port 9; the run fails to connect, which is exit 1
    // after a report, not a usage error.
    let output = wsblast(&[
        "-c",
        "1",
        "-d",
        "200ms",
        "--no-progress",
        "--no-reconnect",
        "ws://127.0.0.1:9",
    ]);
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains("subcommand"), "{stderr}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ws://127.0.0.1:9"),
        "{stderr}"
    );
}

#[test]
fn an_unknown_flag_does_not_mention_subcommands() {
    let output = wsblast(&["ws://127.0.0.1:9", "--no-such-flag"]);
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains("subcommand"), "{stderr}");
}

#[test]
fn a_subcommand_given_first_still_parses() {
    let output = wsblast(&["render", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}
