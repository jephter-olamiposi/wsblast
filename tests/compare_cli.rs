//! `wsblast --compare` run as a process.
//!
//! The reports under `tests/fixtures/compare_legacy/` have the shape wsblast
//! 0.2.0 wrote: no time series and no response time. The `forward.*` and
//! `reverse.*` files are the comparison output of the code before
//! time-series metrics were added, so a comparison of two such reports must
//! stay byte for byte what it was.
#![cfg(feature = "compare")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn wsblast(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wsblast"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compare_legacy")
        .join(name)
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture(name))
        .unwrap()
        .replace("\r\n", "\n")
}

#[test]
fn comparison_of_reports_without_a_time_series_is_unchanged() {
    let baseline = fixture("baseline.json");
    let current = fixture("current.json");
    let (baseline, current) = (baseline.to_str().unwrap(), current.to_str().unwrap());

    for (format, ext) in [("text", "txt"), ("json", "json"), ("markdown", "md")] {
        for (direction, first, second) in [
            ("forward", baseline, current),
            ("reverse", current, baseline),
        ] {
            let output = wsblast(&[
                "--compare",
                first,
                second,
                "--compare-threshold",
                "3",
                "--format",
                format,
            ]);
            assert_eq!(output.status.code(), Some(0), "{direction} {format}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                read_fixture(&format!("{direction}.{ext}")),
                stdout,
                "{direction} {format}"
            );
        }
    }

    let gated = wsblast(&[
        "--compare",
        baseline,
        current,
        "--compare-threshold",
        "3",
        "--fail-on-regression",
    ]);
    assert_eq!(gated.status.code(), Some(2));
}
