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

/// The command runs on a thread with its own 8 MiB stack, so it does not
/// depend on the main thread's: Windows gives that 1 MiB, and an
/// unoptimized build needs more than that to parse the command line. A
/// 256 KiB limit on the main thread stands in for Windows here.
#[cfg(unix)]
#[test]
fn the_command_does_not_need_a_large_main_thread_stack() {
    for args in [&["--version"][..], &["ws://127.0.0.1:1", "-c", "0"]] {
        let output = Command::new("sh")
            .arg("-c")
            .arg(r#"ulimit -s 256 && exec "$0" "$@""#)
            .arg(env!("CARGO_BIN_EXE_wsblast"))
            .args(args)
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        let stderr = stderr(&output);
        assert!(!stderr.contains("overflow"), "{args:?}: {stderr}");
        let expected = if args[0] == "--version" { 0 } else { 1 };
        assert_eq!(output.status.code(), Some(expected), "{args:?}: {stderr}");
    }
}

#[test]
fn arrival_flags_are_checked() {
    for (args, expected) in [
        (
            &[
                "ws://127.0.0.1:1",
                "--arrival-rate",
                "5",
                "--session-duration",
                "1s",
                "-c",
                "3",
            ][..],
            "cannot be used with",
        ),
        (
            &["ws://127.0.0.1:1", "--arrival-rate", "5"],
            "--session-duration",
        ),
        (
            &["ws://127.0.0.1:1", "--session-duration", "1s"],
            "--arrival-rate",
        ),
        (
            &["ws://127.0.0.1:1", "--max-connections", "3"],
            "--arrival-rate",
        ),
        (
            &[
                "ws://127.0.0.1:1",
                "--arrival-rate",
                "0",
                "--session-duration",
                "1s",
            ],
            "--arrival-rate must be greater than zero",
        ),
        (
            &[
                "ws://127.0.0.1:1",
                "--arrival-rate",
                "5",
                "--session-duration",
                "0s",
            ],
            "--session-duration must be greater than zero",
        ),
    ] {
        let output = wsblast(args);
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
}
