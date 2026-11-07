# Command-line reference

The options most runs use, grouped by what they control. `wsblast --help`
lists every option, and the man pages in `contrib/man/` document each
subcommand.

## Target and load

| Option | Default | Description |
| :--- | :--- | :--- |
| `URL` or `-u, --url` | | Target, `ws://` or `wss://` |
| `-c, --connections` | `50` | Concurrent connections |
| `-d, --duration` | `10s` | Total run time, including the ramp |
| `--ramp-rate` | `0` | Connections opened per second (`0` opens all at once) |
| `--stages` | | Load profile instead of `-c`, `-d` and `--ramp-rate`: `DURATION:CONNECTIONS` stages such as `30s:100,2m:100,30s:0` |
| `--arrival-rate` | | New sessions per second instead of `-c`: sessions keep arriving whether or not the server keeps up. Needs `--session-duration`; `-d` sets the length of the run. Cannot be used with `-c`, `--stages` or `--ramp-rate` ([METHODOLOGY.md, section 2.1.2](../METHODOLOGY.md#212-arrival-rate)) |
| `--session-duration` | | How long each `--arrival-rate` session stays connected, handshake included |
| `--max-connections` | | With `--arrival-rate`, the most sessions open at once; arrivals while this many are open are skipped and counted |
| `-r, --rate` | `0` | Messages per second per connection (`0` is unthrottled) |
| `-n, --requests` | | Stop after this many messages in total |
| `--source-ip` | | Local addresses connections bind in turn, comma-separated or repeated, so one machine can open more connections to one target than one address's ephemeral ports allow ([METHODOLOGY.md, section 7.2](../METHODOLOGY.md#72-source-addresses)) |

## Messages

| Option | Default | Description |
| :--- | :--- | :--- |
| `-p, --payload` / `--payload-file` | JSON template | Payload; `{{seq}}`, `{{timestamp}}`, `{{worker_id}}` and `{{data.COLUMN}}` are filled in per frame ([templates](templates.md)) |
| `--data` | | Per-connection test data: a CSV file with a header row, or JSON Lines; connection N uses row N ([templates](templates.md#2-data-files)) |
| `--binary` | | Send binary frames |
| `-H, --header` | | Handshake header, repeatable; the value may hold `{{worker_id}}` and `{{data.COLUMN}}` |
| `--mode` | `echo` | `echo`, `stream`, `listen`, `graphql-ws` or `socketio` |
| `--sio-event`, `--sio-namespace`, `--sio-auth` | `message`, `/`, none | With `--mode socketio`: the event each message is emitted as, the namespace, and the JSON object sent with the namespace connect ([Socket.IO](socketio.md)) |
| `--match` | `first` | Reply matching in echo mode: `first`, `exact` or `json:/path` |
| `--expect` | | Check every echo reply: `json:/pointer`, `json:/pointer=VALUE` or `contains:TEXT`; a reply that fails is a `check_failed` error ([templates](templates.md#3-values-from-replies-and-reply-checks)) |

## SLO gates

| Option | Default | Description |
| :--- | :--- | :--- |
| `--max-p50`, `--max-p95`, `--max-p99`, `--max-p999` | | Latency thresholds (echo mode only): response time with `--rate`, round-trip time without |
| `--max-error-rate` | | Error rate threshold, a fraction such as `0.01` |
| `--min-throughput` | | Messages per second threshold |
| `--max-generator-lag DURATION` | | SLO: exit 2 when the load generator's own p99 wake-up lag exceeds DURATION (the latency numbers would include client-side delay) |

## Output and comparison

| Option | Default | Description |
| :--- | :--- | :--- |
| `--format` / `-o, --output` | `text` | `text`, `json`, `markdown` or `html` (a self-contained page with charts). With `-o FILE` the report goes to FILE and the terminal shows the text summary |
| `--tui` / `--no-progress` | | Live dashboard, or no progress bar for CI logs |
| `--compare BASE CURRENT` | | Compare two JSON reports; `--fail-on-regression` exits with 2 on a regression beyond `--compare-threshold` |
| `--compare ... --format html` | | Comparison as a self-contained HTML page with overlay charts; `wsblast render` also renders a saved comparison JSON |
| `--compare-window START..END` | whole run | Compute the `series.*` comparison metrics only over START..END of each run (`10s..50s`, `10s..`, `..50s`) |

## Authentication

| Option | Default | Description |
| :--- | :--- | :--- |
| `--auth-type` | `none` | `static` (bearer token), `apikey`, `jwt` (`jwt` feature) or `oauth2` (`oauth2` feature) |
| `--auth-token` / `--auth-apikey` | | The bearer token for `static`, or the key for `apikey` |
| `--auth-header` / `--auth-apikey-header` | `Authorization` / `X-API-Key` | Header that carries the token or the key |
| `--auth-token-cmd` | | Command whose trimmed stdout is the token (`--auth-type static`) or API key (`--auth-type apikey`), run before the run starts |
| `--auth-token-cmd-timeout` | `30s` | Time allowed for each run of `--auth-token-cmd` |
| `--auth-token-refresh` | | Re-run `--auth-token-cmd` this often during the run; new handshakes and reconnects use the latest token |
| `--auth-jwt-key FILE` / `--auth-jwt-alg` | `HS256` for `--auth-jwt-alg` | Key file and algorithm for `--auth-type jwt` (`jwt` feature); also `--auth-jwt-claims`, `--auth-jwt-ttl` (default `5m`), `--auth-jwt-kid`, `--auth-jwt-per-connection` |
| `--auth-oauth2-token-url URL` | | OAuth2 token endpoint for `--auth-type oauth2` (`oauth2` feature); also `--auth-oauth2-client-id`, `--auth-oauth2-client-secret-file`, `--auth-oauth2-scope`, `--auth-oauth2-audience` |

## Fault injection

| Option | Default | Description |
| :--- | :--- | :--- |
| `--chaos-delay`, `--chaos-drop`, `--chaos-corrupt`, `--chaos-close` | | Client-side fault injection; `--chaos-close-mode abrupt\|graceful`, `--chaos-seed N` for reproducible runs |
| `--chaos-read-pause DURATION[:JITTER]` | | A slow consumer: pause after every received data frame before reading the next. Works in every mode |

## Capture and live export

| Option | Default | Description |
| :--- | :--- | :--- |
| `--capture FILE` | | Record every data frame and connection event to FILE as JSON lines |
| `--capture-limit N` | `100000` | Most records `--capture` writes; later ones are dropped and counted (`0` = unlimited) |
| `--prometheus-listen` | | Serve live metrics at `http://ADDR/metrics` for Prometheus while the run lasts, e.g. `127.0.0.1:9464` |
| `--otel-endpoint` | | Push live metrics over OTLP/HTTP to this collector base URL, e.g. `http://localhost:4318` (`otel` feature) |
| `--otel-interval` | `10s` | Time between OTLP exports; the final values are also exported when the run ends |
| `--otel-service-name` | `$OTEL_SERVICE_NAME`, else `wsblast` | `service.name` resource attribute of the OTLP export |

## Distributed runs

| Option | Default | Description |
| :--- | :--- | :--- |
| `--agents LIST` | | Generate the load on these agents instead of locally: comma-separated `HOST:PORT`, `ws://HOST:PORT` or `wss://HOST:PORT` |
| `--agent-token TOKEN` | `$WSBLAST_AGENT_TOKEN` | Token the agents were started with |
| `--agent-timeout DURATION` | `30s` | Time the agents get to become ready, and to send their results after the run |
| `--agent-ca PEM` | | Extra CA certificates to trust for `wss://` agents |

## Subcommands

| Option | Default | Description |
| :--- | :--- | :--- |
| `wsblast agent --listen ADDR [--token T] [--tls-cert PEM --tls-key PEM]` | | Run a load agent for a coordinator, one test at a time |
| `wsblast render REPORT.json [--format html\|markdown\|text] [-o FILE]` | `html` | Render a saved JSON report without running a test ([docs/ci.md](https://github.com/jephter-olamiposi/wsblast/blob/main/docs/ci.md#8-html-report)) |
| `wsblast mock [--listen ADDR]` | `127.0.0.1:9001` | Run the mock test server (`--delay`, `--jitter`, `--close-after`, `--reject-ratio`, `--stall-every`/`--stall-for`, `--replay` or `--rules`; `--tls-cert`/`--tls-key` for `wss://`) |

## Exit codes

| Code | Meaning |
| :--- | :--- |
| `0` | The run completed, every SLO passed and at least one connection was established. |
| `1` | Invalid arguments, a configuration or runtime error, or every connection failed. |
| `2` | An SLO failed, or `--fail-on-regression` found a regression. |
