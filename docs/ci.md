# Running wsblast in CI

This guide covers SLO gates, report artifacts and baseline comparison in GitHub Actions, GitLab CI and plain shell. For how each number is measured, see [METHODOLOGY.md](../METHODOLOGY.md). The JSON formats are described by [schema/report-v1.json](../schema/report-v1.json) and [schema/compare-v1.1.json](../schema/compare-v1.1.json).

Contents:

1. [Exit codes](#1-exit-codes)
2. [Installing in CI](#2-installing-in-ci)
3. [Settings for CI runs](#3-settings-for-ci-runs)
4. [GitHub Actions](#4-github-actions)
5. [GitLab CI](#5-gitlab-ci)
6. [Plain shell](#6-plain-shell)
7. [Baseline comparison rules](#7-baseline-comparison-rules)

## 1. Exit codes

| Code | Load test run | `--compare` run |
| :--- | :--- | :--- |
| 0 | The run completed, every SLO passed (or none were set), and at least one connection was established. | The comparison completed. Without `--fail-on-regression`, this is the result even when regressions were found. |
| 1 | Configuration or runtime error: bad URL, scheme, duration or header, latency SLO outside `--mode echo`, invalid scenario, unreadable payload file, report write failure. Also: every connection slot failed and no SLO failed. | A report file is missing or is not a wsblast report. |
| 2 | At least one SLO failed. | `--fail-on-regression` is set and at least one metric regressed beyond the threshold. |

Rules that matter in a pipeline:

- **Exit code 2 is also used by the argument parser** for unknown flags, missing values and conflicting flags (for example `--scenario` with `-c`, or a negative number passed as `--compare-threshold -1`). A typo in a CI command therefore looks like an SLO breach. To tell them apart, check that the report file exists and read `slo.passed` from it (see [section 6](#6-plain-shell)).
- SLOs are checked before the "all connections failed" condition. If every connection fails and an SLO also fails (for example `--max-error-rate`), the exit code is 2.
- After a completed run the report is printed to stdout and written to the `-o` file before the process exits, also when it exits with 1 or 2. Upload it with an "always" condition.
- A latency SLO with no RTT samples fails ("no RTT samples"). An unreachable target cannot pass a latency gate.

## 2. Installing in CI

`--compare` and `--scenario` are cargo features. Install with the ones you need:

```bash
cargo install wsblast --locked --features compare,scenario
```

The pre-built release binaries and the `install.sh` script ship the default feature set. Check with `wsblast --help` that `--compare` or `--scenario` is listed before relying on them; if not, use `cargo install` as above.

Static tokens and API keys can always be sent as plain handshake headers, which need no feature:

```bash
wsblast "$TARGET" -H "Authorization: Bearer $WS_TOKEN"
wsblast "$TARGET" -H "X-API-Key: $WS_API_KEY"
```

## 3. Settings for CI runs

| Setting | Why |
| :--- | :--- |
| `--no-progress` | The progress bar is meant for a terminal. It also adds up to about 100 ms to the measured elapsed time. |
| No `--tui` | The TUI needs an interactive terminal. |
| An explicit `-d` (and `-c`, `-r`) | Baseline comparison is only meaningful between runs with the same settings. |
| `--format json -o report.json` | The JSON report is the stable, machine-readable record. Keep it as an artifact. |
| `--rate` set | A paced run is more repeatable than an unthrottled one, whose load depends on the server's speed. |
| A threshold with headroom | Shared CI runners are noisy. Gate on the latency your users need, not on the best run you have seen. |

Remember that latency SLOs (`--max-p50`, `--max-p95`, `--max-p99`, `--max-p999`) need `--mode echo` (the default). Stream and listen mode can still be gated on `--max-error-rate` and `--min-throughput`.

wsblast writes one format per run. To get both a machine-readable record and a readable summary from one run, write JSON and render the summary from it with `jq`, as in the recipes below.

## 4. GitHub Actions

### 4.1 SLO gate with a job summary and a JSON artifact

```yaml
name: websocket-perf

on:
  pull_request:
  workflow_dispatch:

jobs:
  slo-gate:
    runs-on: ubuntu-latest
    env:
      TARGET_URL: ${{ vars.WSBLAST_TARGET_URL }}   # for example wss://staging.example.com/ws
    steps:
      - uses: actions/checkout@v4

      - name: Cache cargo binaries
        uses: actions/cache@v4
        with:
          path: ~/.cargo/bin/wsblast
          key: wsblast-${{ runner.os }}-features-compare-scenario

      - name: Install wsblast
        run: command -v wsblast || cargo install wsblast --locked --features compare,scenario

      - name: Run load test with SLO gate
        env:
          WS_TOKEN: ${{ secrets.WS_TOKEN }}
        run: |
          wsblast "$TARGET_URL" \
            -c 50 -d 30s -r 20 \
            -H "Authorization: Bearer $WS_TOKEN" \
            --max-p99 50ms \
            --max-error-rate 0.01 \
            --no-progress \
            --format json -o report.json > /dev/null

      - name: Write job summary
        if: always() && hashFiles('report.json') != ''
        run: |
          jq -r '
            "### wsblast: \(.target_url)",
            "",
            "| Metric | Value |",
            "| :--- | ---: |",
            "| Connections established / attempted | \(.metrics.total_connections_established) / \(.metrics.total_connections_attempted) |",
            "| Reconnects | \(.metrics.total_reconnects // 0) |",
            "| Messages sent / received | \(.metrics.total_messages_sent) / \(.metrics.total_messages_recv) |",
            "| Throughput (msg/s) | \(.metrics.throughput_msg_per_sec | floor) |",
            "| Error rate | \((.metrics.error_rate * 10000 | round) / 100) % |",
            "| RTT p50 / p99 / p99.9 (µs) | \(.metrics.message_rtt.p50_us) / \(.metrics.message_rtt.p99_us) / \(.metrics.message_rtt.p999_us) |",
            "",
            (if (.slo.checks | length) > 0 then
               "| SLO | Target | Actual | Result |",
               "| :--- | :--- | :--- | :--- |",
               (.slo.checks[] | "| \(.name) | `\(.target)` | `\(.actual)` | \(if .passed then "pass" else "**FAIL**" end) |")
             else empty end)
          ' report.json >> "$GITHUB_STEP_SUMMARY"

      - name: Archive JSON report
        if: always() && hashFiles('report.json') != ''
        uses: actions/upload-artifact@v4
        with:
          name: wsblast-report
          path: report.json
          retention-days: 30
```

The gate step fails the job on exit code 1 or 2. The summary and upload steps still run because of `always()`, and are skipped when no report was written (for example after a configuration error).

If you only need the readable report, let wsblast write Markdown directly:

```yaml
      - name: Run load test with SLO gate
        run: |
          wsblast "$TARGET_URL" -c 50 -d 30s --max-p99 50ms --max-error-rate 0.01 \
            --no-progress --format markdown -o report.md > /dev/null

      - name: Write job summary
        if: always() && hashFiles('report.md') != ''
        run: cat report.md >> "$GITHUB_STEP_SUMMARY"
```

### 4.2 Baseline comparison against a committed baseline

The simplest baseline is a JSON report committed to the repository, for example `perf/baseline.json`, refreshed on purpose when performance changes are accepted.

```yaml
      - name: Run load test
        run: |
          wsblast "$TARGET_URL" -c 50 -d 30s -r 20 \
            --no-progress --format json -o current.json > /dev/null

      - name: Compare with baseline
        run: |
          wsblast --compare perf/baseline.json current.json \
            --compare-threshold 15 \
            --format markdown -o compare.md \
            --fail-on-regression

      - name: Add comparison to job summary
        if: always() && hashFiles('compare.md') != ''
        run: cat compare.md >> "$GITHUB_STEP_SUMMARY"
```

To refresh the baseline, run the same command locally or in a manual workflow and commit the new `current.json` as `perf/baseline.json`.

### 4.3 Baseline comparison against the last run on `main`

This variant stores each `main` run as an artifact and compares pull requests against the newest one. It uses the `gh` CLI that GitHub-hosted runners include.

```yaml
name: websocket-perf

on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read
  actions: read

jobs:
  perf:
    runs-on: ubuntu-latest
    env:
      TARGET_URL: ${{ vars.WSBLAST_TARGET_URL }}
    steps:
      - uses: actions/checkout@v4

      - name: Install wsblast
        run: cargo install wsblast --locked --features compare

      - name: Run load test
        run: |
          wsblast "$TARGET_URL" -c 50 -d 30s -r 20 \
            --max-error-rate 0.01 \
            --no-progress --format json -o current.json > /dev/null

      - name: Store baseline (main only)
        if: github.event_name == 'push'
        uses: actions/upload-artifact@v4
        with:
          name: wsblast-baseline
          path: current.json
          retention-days: 90

      - name: Download baseline from main
        if: github.event_name == 'pull_request'
        id: baseline
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          run_id=$(gh run list --repo "$GITHUB_REPOSITORY" --workflow websocket-perf.yml \
            --branch main --event push --status success --limit 1 \
            --json databaseId --jq '.[0].databaseId')
          if [ -z "$run_id" ]; then
            echo "No baseline run on main yet; skipping comparison."
            exit 0
          fi
          gh run download "$run_id" --repo "$GITHUB_REPOSITORY" \
            --name wsblast-baseline --dir baseline
          echo "found=true" >> "$GITHUB_OUTPUT"

      - name: Compare with baseline
        if: steps.baseline.outputs.found == 'true'
        run: |
          wsblast --compare baseline/current.json current.json \
            --compare-threshold 15 --format markdown -o compare.md
          cat compare.md >> "$GITHUB_STEP_SUMMARY"
          wsblast --compare baseline/current.json current.json \
            --compare-threshold 15 --format json -o compare.json > /dev/null
          # Fail only on the metrics this project gates on.
          jq -e '[.regressions[].metric]
                 | map(select(. == "message_rtt.p99_us" or . == "error_rate" or . == "throughput_msg_per_sec"))
                 | length == 0' compare.json

      - name: Archive reports
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: wsblast-reports
          path: |
            current.json
            compare.json
          if-no-files-found: ignore
```

`--fail-on-regression` fails on any regressed metric, including counters and every percentile. The last step instead reads the comparison JSON and fails only on a chosen set of metrics: `jq -e` exits with 1 when the expression is false. Metric names are listed in [section 7](#7-baseline-comparison-rules).

The `--workflow` value must match this workflow's file name under `.github/workflows/`.

## 5. GitLab CI

```yaml
stages:
  - perf

websocket-slo:
  stage: perf
  image: rust:1.85
  variables:
    TARGET_URL: "wss://staging.example.com/ws"
  cache:
    key: wsblast-compare-scenario
    paths:
      - .cargo-bin/
  before_script:
    - apt-get update -qq && apt-get install -y -qq jq > /dev/null
    - |
      if [ ! -x .cargo-bin/bin/wsblast ]; then
        cargo install wsblast --locked --features compare,scenario --root .cargo-bin
      fi
    - export PATH="$PWD/.cargo-bin/bin:$PATH"
  script:
    - |
      wsblast "$TARGET_URL" \
        -c 50 -d 30s -r 20 \
        -H "Authorization: Bearer $WS_TOKEN" \
        --max-p99 50ms --max-error-rate 0.01 \
        --no-progress --format json -o report.json > /dev/null
    - |
      if [ -f perf/baseline.json ]; then
        wsblast --compare perf/baseline.json report.json \
          --compare-threshold 15 --format text --fail-on-regression
      fi
  after_script:
    - |
      if [ -f report.json ]; then
        jq -r '"p99 \(.metrics.message_rtt.p99_us) us, error rate \(.metrics.error_rate), slo passed: \(.slo.passed)"' report.json
      fi
  artifacts:
    when: always
    paths:
      - report.json
    expire_in: 30 days
```

`WS_TOKEN` comes from a masked CI/CD variable. The job fails on a non-zero exit from either command. `artifacts: when: always` keeps `report.json` when the gate fails.

To let only an SLO breach pass with a warning while configuration errors still fail the pipeline, add:

```yaml
  allow_failure:
    exit_codes: [2]
```

Keep in mind that exit code 2 also covers argument errors (see [section 1](#1-exit-codes)), so do this only for a command line you have tested.

## 6. Plain shell

A script for any runner, or for a cron job on a load-generator host. It separates the three outcomes, and uses the report file to tell an SLO breach from an argument error.

```bash
#!/usr/bin/env bash
set -u

TARGET_URL="${TARGET_URL:-ws://127.0.0.1:9001}"
OUT_DIR="${OUT_DIR:-wsblast-out}"
mkdir -p "$OUT_DIR"
report="$OUT_DIR/report.json"
rm -f "$report"

wsblast "$TARGET_URL" \
  -c 50 -d 30s -r 20 \
  --max-p99 50ms --max-error-rate 0.01 \
  --no-progress --format json -o "$report" > /dev/null
status=$?

case "$status" in
  0)
    echo "PASS: all SLOs met"
    ;;
  2)
    if [ -f "$report" ] && [ "$(jq -r '.slo.passed' "$report")" = "false" ]; then
      echo "FAIL: SLO breached"
      jq -r '.slo.checks[] | select(.passed | not) | "  \(.name): \(.actual) (target \(.target))"' "$report"
    else
      echo "ERROR: wsblast rejected its arguments (exit 2, no report)"
    fi
    ;;
  *)
    echo "ERROR: wsblast failed with exit code $status"
    ;;
esac

if [ -f "$OUT_DIR/baseline.json" ] && [ -f "$report" ]; then
  wsblast --compare "$OUT_DIR/baseline.json" "$report" \
    --compare-threshold 15 --fail-on-regression
  compare_status=$?
  [ "$compare_status" -ne 0 ] && [ "$status" -eq 0 ] && status=$compare_status
fi

exit "$status"
```

## 7. Baseline comparison rules

`wsblast --compare BASELINE CURRENT` reads two JSON reports. Each may be a full report (`--format json` output) or a bare metrics object.

For each metric it computes `pct_change = (current - baseline) / baseline * 100` and reports the metric only when `|pct_change|` is strictly greater than `--compare-threshold` (default 10):

| Direction | Metrics |
| :--- | :--- |
| Higher is better | `total_connections_established`, `total_messages_sent`, `total_messages_recv`, `total_bytes_sent`, `total_bytes_recv`, `throughput_msg_per_sec`, `throughput_bytes_per_sec` |
| Lower is better | `total_connections_failed`, `error_rate`, `errors.<category>` (one per error category, for example `errors.timeout`), `handshake_latency.p50_us`, `.p95_us`, `.p99_us`, `.p999_us`, `message_rtt.p50_us`, `.p95_us`, `.p99_us`, `.p999_us` |
| Not compared | `total_connections_attempted`, `total_reconnects`, mean, min, max, p75, p90, p99.99 |

Edge cases:

- A metric that is 0 in both reports is skipped.
- A metric that is 0 in the baseline and non-zero in the current report is reported with `pct_change = 100`. A new error category is therefore always a regression when the threshold is below 100.
- A metric that drops to 0 has `pct_change = -100`. For a higher-is-better metric that is a regression.
- The comparison does not check that both runs used the same settings. Counters such as `total_messages_sent` scale with `-c`, `-d` and `-r`, so a shorter run shows up as a regression. Compare runs made with the same command line.
- Compare runs of the same mode. A stream or listen report has an empty RTT histogram (all percentiles 0), so comparing it with an echo report flags every RTT percentile.
- The comparison JSON (`--format json`) lists only the regressions and improvements, not the two reports. Archive the reports themselves if you need the raw values later.

A local check: comparing a clean echo report with one from a run where every request timed out listed `error_rate` (0 to 0.5) and `errors.timeout` (0 to 51) as regressions with `pct_change` 100; `--fail-on-regression` made the same command exit with 2, and without it the exit code was 0.
