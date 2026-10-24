//! The man page and shell completions shipped in the release archives are
//! generated from the CLI definition and committed under `contrib/`. This
//! test fails when they drift from the binary; regenerate them with
//! `WSBLAST_BLESS=1 cargo test --test generated_assets`.
//!
//! The committed files describe the default build, which enables every
//! feature, so the comparison only runs when all of them are enabled.
#![cfg(all(
    feature = "scenario",
    feature = "compare",
    feature = "auth",
    feature = "graphql-ws"
))]

use std::path::Path;
use std::process::Command;

const ASSETS: &[(&[&str], &str)] = &[
    (&["man"], "contrib/man/wsblast.1"),
    (&["completions", "bash"], "contrib/completions/wsblast.bash"),
    (&["completions", "zsh"], "contrib/completions/_wsblast"),
    (&["completions", "fish"], "contrib/completions/wsblast.fish"),
    (
        &["completions", "powershell"],
        "contrib/completions/_wsblast.ps1",
    ),
    (
        &["completions", "elvish"],
        "contrib/completions/wsblast.elv",
    ),
];

fn generate(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "wsblast {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn committed_man_page_and_completions_are_up_to_date() {
    let bless = std::env::var_os("WSBLAST_BLESS").is_some();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut stale = Vec::new();

    for (args, relative) in ASSETS {
        let generated = generate(args);
        let path = root.join(relative);
        if bless {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &generated).unwrap();
            continue;
        }
        // A Windows checkout may convert the committed files to CRLF.
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        if committed != generated.replace("\r\n", "\n") {
            stale.push(*relative);
        }
    }

    assert!(
        stale.is_empty(),
        "out of date: {stale:?}; run `WSBLAST_BLESS=1 cargo test --test generated_assets`"
    );
}
