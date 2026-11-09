//! `wsblast import` on HAR files Chrome 153 exported with its DevTools HAR
//! exporter (the Network panel's), after recording a plain WebSocket page
//! and a Socket.IO 4 page that started on long-polling and upgraded.
#![cfg(feature = "scenario")]

use std::path::PathBuf;
use std::process::{Command, Output};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/har")
        .join(name)
}

/// Runs `wsblast import FIXTURE -o OUT`; returns the output and the YAML.
fn import(name: &str) -> (Output, String) {
    let out = std::env::temp_dir().join(format!(
        "wsblast-har-import-{}-{name}.yaml",
        std::process::id()
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .arg("import")
        .arg(fixture(name))
        .arg("-o")
        .arg(&out)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let yaml = std::fs::read_to_string(&out).unwrap_or_default();
    let _ = std::fs::remove_file(&out);
    (output, yaml)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_chrome_recording_of_a_plain_session_becomes_its_scenario() {
    let (output, yaml) = import("chrome-plain.har");
    let stderr = stderr(&output);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("4 messages to ws://127.0.0.1:19790/chat?room=lobby"),
        "{stderr}"
    );
    assert!(
        stderr.contains("1 binary message the browser sent is left out"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Run it with: wsblast --scenario"),
        "{stderr}"
    );
    // The page paused 400 ms and 250 ms after replies, then sent a binary
    // message (left out) 250 ms after a reply and "leave" 300 ms after its
    // reply: one pause of the whole 551 ms.
    for pause in ["duration: 400ms", "duration: 250ms", "duration: 551ms"] {
        assert!(yaml.contains(pause), "{pause}: {yaml}");
    }
    assert!(
        yaml.contains(r#"payload: '{"op":"join","room":"lobby"}'"#),
        "{yaml}"
    );
    assert!(yaml.contains(r#"payload: '{"op":"leave"}'"#), "{yaml}");
}

#[test]
fn a_chrome_recording_of_an_upgraded_socketio_session_names_its_auth() {
    let (output, yaml) = import("chrome-socketio-upgrade.har");
    let stderr = stderr(&output);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("--mode socketio --sio-event chat --sio-namespace /files"),
        "{stderr}"
    );
    // The namespace connect and its auth object went over long-polling,
    // before the upgrade; the token itself is never copied.
    assert!(stderr.contains("auth object (token)"), "{stderr}");
    assert!(
        stderr.contains("left out 'typing' in /files (1)"),
        "{stderr}"
    );
    assert!(!yaml.contains("secret-token"), "{yaml}");
    assert!(
        yaml.contains("target: ws://127.0.0.1:19793/socket.io/\n"),
        "{yaml}"
    );
    assert_eq!(yaml.matches("type: Send").count(), 4, "{yaml}");
    assert_eq!(yaml.matches("duration: 200ms").count(), 3, "{yaml}");
}
