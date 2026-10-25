# Running wsblast in CI

This guide covers SLO gates, report artifacts and baseline comparison in GitHub Actions, GitLab CI and plain shell. For how each number is measured, see [METHODOLOGY.md](../METHODOLOGY.md). The JSON formats are described by [schema/report-v1.json](../schema/report-v1.json) and [schema/compare-v1.2.json](../schema/compare-v1.2.json) (which also covers the 1.1 documents of wsblast 0.2.0).

Contents:

1. [Exit codes](#1-exit-codes)
2. [Installing in CI](#2-installing-in-ci)
3. [Settings for CI runs](#3-settings-for-ci-runs)
4. [GitHub Actions](#4-github-actions)
5. [GitLab CI](#5-gitlab-ci)
6. [Plain shell](#6-plain-shell)
7. [Baseline comparison rules](#7-baseline-comparison-rules)
8. [HTML report](#8-html-report)
9. [HTML comparison](#9-html-comparison)

## 1. Exit codes

| Code | Load test run | `--compare` run |
| :--- | :--- | :--- |
| 0 | The run completed, every SLO passed (or none were set), and at least one connection was established. | The comparison completed. Without `--fail-on-regression`, this is the result even when regressions were found. |
| 1 | Invalid command-line arguments: unknown flag, missing value, conflicting flags (for example `--scenario` with `-c`). Configuration or runtime error: bad URL, scheme, duration or header, latency SLO outside `--mode echo`, invalid scenario, unreadable payload file, report write failure. Also: every connection slot failed and no SLO failed. | Invalid arguments, or a report file is missing or is not a wsblast report. |
| 2 | At least one SLO failed. | `--fail-on-regression` is set and at least one metric regressed beyond the threshold. |

Rules that matter in a pipeline:

- **Exit code 2 always means an SLO breach** (or a regression with `--fail-on-regression`). A typo in a CI command, such as an unknown flag or a missing value, exits with 1 like any other configuration error. `--help` and `--version` exit with 0.
- SLOs are checked before the "all connections failed" condition. If every connection fails and an SLO also fails (for example `--max-error-rate`), the exit code is 2.
- After a completed run the report is printed to stdout and written to the `-o` file before the process exits, also when it exits with 1 or 2. Upload it with an "always" condition.
- A latency SLO with no RTT samples fails ("no RTT samples"). An unreachable target cannot pass a latency gate.

## 2. Installing in CI

Every wsblast feature except the opt-in OpenTelemetry export, including `--compare`, `--scenario`, `--auth-type`, `--mode graphql-ws` and `--prometheus-listen`, is part of the default build, so a plain install has all of them (add `--features otel` for `--otel-endpoint`, see [docs/export.md](export.md)):

```bash
cargo install wsblast --locked
```

The pre-built release binaries, `cargo binstall wsblast` and the `install.sh` script ship the same default build. wsblast 0.1 binaries did not include `--compare` or `--scenario`; check `wsblast --version` if a runner may have an old copy cached.

Static tokens and API keys can always be sent as plain handshake headers, which need no feature:

```bash
wsblast "$TARGET" -H "Authorization: Bearer $WS_TOKEN"
wsblast "$TARGET" -H "X-API-Key: $WS_API_KEY"
```

### 2.1 Tokens from a command

With the `auth` feature, `--auth-token-cmd` gets the token from a command instead of the command line, which keeps it out of the job log and the process list and lets the job mint a fresh one:

```bash
wsblast "$TARGET" --auth-type static --auth-token-cmd "gcloud auth print-identity-token"
wsblast "$TARGET" --auth-type apikey --auth-apikey-header X-API-Key \
  --auth-token-cmd "./scripts/mint-key.sh"
```

- The command runs through the platform shell (`sh -c` on Unix, `cmd /C` on Windows) before the run starts. Its standard output, with leading and trailing whitespace removed, is used exactly as `--auth-token` (with `--auth-type static`) or `--auth-apikey` (with `--auth-type apikey`) would be: `Authorization` gets a `Bearer ` prefix unless the token has one, and `--auth-header` and `--auth-apikey-header` name the header as usual. It cannot be combined with `--auth-token` or `--auth-apikey`. Its standard input is empty, and its standard error is captured: only its first line is shown, and only when the command fails.
- `--auth-token-cmd-timeout` (default `30s`) bounds each run of the command; a command still running then is killed.
- A command that cannot be started, times out, exits with a non-zero status, prints nothing, or prints something that is not a valid HTTP header value is a configuration error: wsblast exits with code 1 before any connection is opened. The message gives the exit status and the first line of the command's standard error. The token itself is never printed, in errors or reports.
- `--auth-token-refresh DURATION` runs the command again during the run, each time `DURATION` after the previous run finished. New handshakes (connections still ramping up, connections a stage adds, and reconnects) use the latest token; connections that are already open are not touched. A failed refresh keeps the previous token and prints a warning to stderr; while refreshes keep failing, at most one warning per minute is printed, with the count of failures it covers. A refresh still running when the run ends is abandoned.

When a run can outlast the token and connections may reconnect, set the refresh interval below the token's lifetime.

## 3. Settings for CI runs

| Setting | Why |
| :--- | :--- |
| `--no-progress` | The progress bar is meant for a terminal. It also adds up to about 100 ms to the measured elapsed time. |
| No `--tui` | The TUI needs an interactive terminal. |
| An explicit `-d` (and `-c`, `-r`) | Baseline comparison is only meaningful between runs with the same settings. |
| `--format json -o report.json` | The JSON report is the stable, machine-readable record. Keep it as an artifact. |
| `--rate` set | A paced run is more repeatable than an unthrottled one, whose load depends on the server's speed. |
| A threshold with headroom | Shared CI runners are noisy. Gate on the latency your users need, not on the best run you have seen. |

Remember that latency SLOs (`--max-p50`, `--max-p95`, `--max-p99`, `--max-p999`) need `--mode echo` (the default). With `--rate` they gate on response time, which includes the time a request waited behind a slow reply; without it they gate on round-trip time. Set `--rate` for a gate that catches server stalls. Stream and listen mode can still be gated on `--max-error-rate` and `--min-throughput`.

wsblast writes one format per run. To get both a machine-readable record and a readable summary from one run, write JSON and render the summary from it, either with `wsblast render` ([section 8](#8-html-report)) or with `jq`, as in the recipes below.

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
          key: wsblast-${{ runner.os }}

      - name: Install wsblast
        run: command -v wsblast || cargo install wsblast --locked

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
        run: cargo install wsblast --locked

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

To gate on the steady state of the run instead, and catch a stall or an error burst that the totals average away, compare the time-series metrics over a window that leaves out the ramp and the ramp-down. This step fails only when the worst interval's p99 or the mean steady-state throughput regressed; both reports need a time series, so the baseline must come from wsblast 0.3 or later:

```yaml
      - name: Compare steady state with baseline
        if: steps.baseline.outputs.found == 'true'
        run: |
          wsblast --compare baseline/current.json current.json \
            --compare-window 10s..50s --compare-threshold 15 \
            --format json -o compare-steady.json > /dev/null
          jq -e '[.regressions[].metric]
                 | map(select(. == "series.p99_worst_us" or . == "series.throughput_mean"))
                 | length == 0' compare-steady.json
```

With `-d 60s`, `10s..50s` skips the first 10 seconds and the last 10. The window applies only to the `series.*` metrics; the whole-run metrics in the same JSON are unchanged by it.

The `--workflow` value must match this workflow's file name under `.github/workflows/`.

## 5. GitLab CI

```yaml
stages:
  - perf

websocket-slo:
  stage: perf
  image: rust:1.88
  variables:
    TARGET_URL: "wss://staging.example.com/ws"
  cache:
    key: wsblast
    paths:
      - .cargo-bin/
  before_script:
    - apt-get update -qq && apt-get install -y -qq jq > /dev/null
    - |
      if [ ! -x .cargo-bin/bin/wsblast ]; then
        cargo install wsblast --locked --root .cargo-bin
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

Argument errors exit with 1 (see [section 1](#1-exit-codes)), so a typo in the command still fails the pipeline.

## 6. Plain shell

A script for any runner, or for a cron job on a load-generator host. It separates the three outcomes by exit code.

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
    echo "FAIL: SLO breached"
    jq -r '.slo.checks[] | select(.passed | not) | "  \(.name): \(.actual) (target \(.target))"' "$report"
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

| Flag | Default | Effect |
| :--- | :--- | :--- |
| `--compare-threshold PERCENT` | 10 | A metric is reported when its relative change is strictly greater than this. |
| `--fail-on-regression` | off | Exit with 2 when any metric regressed. |
| `--compare-window START..END` | whole run | Compute the `series.*` metrics only over the intervals that lie fully inside START..END of each run, for example `10s..50s`; either side may be left out (`10s..` skips the warm-up, `..50s` drops the tail). START must be before END. With a window, a report without a time series or without an interval inside the window is an error (exit 1). Whole-run metrics ignore it. |

For each metric it computes `pct_change = (current - baseline) / baseline * 100` and reports the metric only when `|pct_change|` is strictly greater than `--compare-threshold` (default 10):

| Direction | Metrics |
| :--- | :--- |
| Higher is better | `total_connections_established`, `total_messages_sent`, `total_messages_recv`, `total_bytes_sent`, `total_bytes_recv`, `throughput_msg_per_sec`, `throughput_bytes_per_sec` |
| Lower is better | `total_connections_failed`, `error_rate`, `errors.<category>` (one per error category, for example `errors.timeout`), `handshake_latency.p50_us`, `.p95_us`, `.p99_us`, `.p999_us`, `message_rtt.p50_us`, `.p95_us`, `.p99_us`, `.p999_us`, and for paced runs `response_time.p50_us`, `.p95_us`, `.p99_us`, `.p999_us` (compared only when both reports have them) |
| Higher is better, from the time series | `series.throughput_mean` |
| Lower is better, from the time series | `series.throughput_cv`, `series.p99_median_us`, `series.p99_worst_us`, `series.error_intervals`, `series.max_interval_errors` |
| Not compared | `total_connections_attempted`, `total_reconnects`, mean, min, max, p75, p90, p99.99 |

The `series.*` metrics summarise each report's time series (one point per interval, see [METHODOLOGY.md section 6.3](../METHODOLOGY.md#63-time-series)) over the `--compare-window`, or the whole run without one:

| Metric | Definition |
| :--- | :--- |
| `series.throughput_mean` | Mean of the per-interval `throughput_msg_per_sec`, weighted by interval duration, over the full-length intervals (the shorter last interval of a run or scenario phase is left out). |
| `series.throughput_cv` | Coefficient of variation (standard deviation divided by mean, duration-weighted) of the same per-interval throughput: 0 when it is steady. Left out with fewer than two full-length intervals or a mean of 0. |
| `series.p99_median_us` | Median of the interval p99 latencies, over the intervals with latency samples. |
| `series.p99_worst_us` | Highest interval p99 latency. |
| `series.error_intervals` | Number of intervals with at least one error. |
| `series.max_interval_errors` | Most errors in one interval. |

They are compared only when both reports have a time series. A comparison with a report from wsblast 0.2.0 or earlier, which has none, leaves them out and prints exactly what 0.2.0 printed. A metric that is undefined for one report (see the table) is also left out. Each metric summarises one report, so reports with different interval lengths can be compared. [METHODOLOGY.md section 6.5](../METHODOLOGY.md#65-time-series-metrics-in-comparisons) has the exact definitions.

Edge cases:

- A metric that is 0 in both reports is skipped.
- A metric that is 0 in the baseline and non-zero in the current report is reported with `pct_change = 100`. A new error category is therefore always a regression when the threshold is below 100.
- A metric that drops to 0 has `pct_change = -100`. For a higher-is-better metric that is a regression.
- The comparison does not check that both runs used the same settings. Counters such as `total_messages_sent` scale with `-c`, `-d` and `-r`, so a shorter run shows up as a regression. Compare runs made with the same command line.
- Compare runs of the same mode. A stream or listen report has an empty RTT histogram (all percentiles 0), so comparing it with an echo report flags every RTT percentile.
- The comparison JSON (`--format json`) lists only the regressions and improvements, not the two reports. Archive the reports themselves if you need the raw values later.

`--compare` writes `--format text` (the default), `json`, `markdown` or `html`; [section 9](#9-html-comparison) describes the HTML page.
- `series.throughput_cv` and the error metrics are 0 or close to it in a healthy run, so a tiny absolute change is a large relative one, and a first error interval is a 100% change. Gate on them with care, or select the metrics a gate fails on with `jq` as in [section 4.3](#43-baseline-comparison-against-the-last-run-on-main).
- Schema version: the comparison JSON says `"schema_version": "1.2.0"` and includes a `window` object when `--compare-window` is set, and says 1.2.0 when both reports have a time series. A comparison of two reports without a time series and without a window is written as `"1.1.0"`, byte for byte as wsblast 0.2.0 wrote it. Readers of 1.1 keep working: 1.2 only adds metric names and the optional `window` property. See [schema/compare-v1.2.json](../schema/compare-v1.2.json).

A local check: comparing a clean echo report with one from a run where every request timed out listed `error_rate` (0 to 1) and `errors.timeout` (0 to 51) as regressions with `pct_change` 100; `--fail-on-regression` made the same command exit with 2, and without it the exit code was 0.

## 8. HTML report

`--format html` writes the report as one self-contained HTML file: the run's settings, a pass or fail banner with every SLO check, the key numbers (throughput, error rate, p50 and p99 of the latency the gate reads), the latency percentiles, the error breakdown, and four charts of the time series: throughput, p99 latency, active connections and errors per interval. The charts are inline SVG and the styles are inline CSS. The page loads nothing from the network and runs no script, so it opens offline, from a downloaded CI artifact, and in light or dark mode. Each chart has a text description and a table of its points for screen readers and for copying values.

```bash
wsblast "$TARGET_URL" -c 50 -d 30s -r 20 --max-p99 50ms \
  --no-progress --format html -o report.html > /dev/null
```

The JSON report stays the machine-readable record, so a CI job usually writes JSON and turns it into HTML afterwards with `wsblast render`, which reads a saved report and renders it without running a test:

```bash
wsblast render report.json -o report.html               # HTML (the default)
wsblast render report.json --format markdown -o report.md
wsblast render report.json --format text                # terminal tables on stdout
```

`wsblast render` reads any schema 1.x report, including reports written by wsblast 0.2.0. Parts an older report lacks are left out of the page; a report without a time series has no charts. The JSON report does not record `--rate`, `--stages` or the run's date, so the rendered page shows the rate as not recorded and the date as the time the JSON file was last modified. Without `-o` the rendered report is printed to stdout; with `-o` it is written to the file only.

Exit codes of `wsblast render`: 0 when the report was rendered, whatever its SLO result; 1 when the file is missing or unreadable, is not JSON, is neither a wsblast report nor a comparison JSON, or has a schema version other than 1.x, or when the output file cannot be written. Gate on the run's own exit code, not on `render`.

In GitHub Actions, render the HTML in an `always()` step and upload it next to the JSON report:

```yaml
      - name: Run load test with SLO gate
        run: |
          wsblast "$TARGET_URL" -c 50 -d 30s -r 20 \
            --max-p99 50ms --max-error-rate 0.01 \
            --no-progress --format json -o report.json > /dev/null

      - name: Render HTML report
        if: always() && hashFiles('report.json') != ''
        run: wsblast render report.json -o report.html

      - name: Archive reports
        if: always() && hashFiles('report.json') != ''
        uses: actions/upload-artifact@v4
        with:
          name: wsblast-report
          path: |
            report.json
            report.html
          retention-days: 30
```

Download the artifact from the run's summary page and open `report.html` in a browser. A stored JSON report, for example a baseline from an earlier run, can be rendered the same way at any time later. In GitLab CI, add `report.html` to `artifacts: paths`.

## 9. HTML comparison

`wsblast --compare BASELINE CURRENT --format html` writes the comparison as one self-contained page in the style of the run report: inline CSS and SVG, no script, nothing loaded from the network, light and dark mode, and every string from the reports (file names, metric names) HTML-escaped. It shows:

- the two report files, the threshold and the `--compare-window`, if set;
- a verdict banner: the number of regressions and improvements beyond the threshold, and the exit code `--fail-on-regression` gives;
- a table of every compared metric, with the baseline and current values in their units, the signed change, the direction that is better and the result. Regressions are highlighted and listed first, then improvements, then the metrics within the threshold. Metrics that are 0 in both reports are left out, as in the comparison itself ([section 7](#7-baseline-comparison-rules));
- when both reports have a time series, two charts that overlay the baseline (dashed) and current (solid) runs: throughput and p99 latency per interval, each against the time since its run started. Runs with different lengths or interval lengths share one time axis, and the comparison window is shaded. Each chart has a text description and a table of both runs' points. Reports from wsblast 0.2.0 and earlier have no time series, so their comparison page has no charts.

The flags, the exit code and `-o` work as for the other formats: the page is printed to stdout and also written to the `-o` file.

```bash
wsblast --compare perf/baseline.json current.json \
  --compare-threshold 15 --compare-window 10s..50s \
  --format html -o compare.html > /dev/null
```

`wsblast render` also reads a comparison JSON (schema 1.1 or 1.2, as written by `--compare --format json`) and renders it as HTML (the default), Markdown or text. The Markdown and text are what `--compare` printed in those formats. A comparison JSON holds only the metrics that changed beyond the threshold and no time series, so its HTML page lists those metrics and has no charts; render from the two reports for the full table and the overlay. The exit code of `render` does not reflect regressions.

```bash
wsblast --compare perf/baseline.json current.json --format json -o compare.json > /dev/null
wsblast render compare.json -o compare.html
```

In GitHub Actions, write the page in an `always()` step after the gate and upload it with the reports:

```yaml
      - name: Render HTML comparison
        if: always() && hashFiles('current.json') != ''
        run: |
          wsblast --compare perf/baseline.json current.json \
            --compare-threshold 15 --format html -o compare.html > /dev/null

      - name: Upload comparison
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: websocket-comparison
          path: |
            current.json
            compare.html
```

