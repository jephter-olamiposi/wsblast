
use builtin;
use str;

set edit:completion:arg-completer[wsblast] = {|@words|
    fn spaces {|n|
        builtin:repeat $n ' ' | str:join ''
    }
    fn cand {|text desc|
        edit:complex-candidate $text &display=$text' '(spaces (- 14 (wcswidth $text)))$desc
    }
    var command = 'wsblast'
    for word $words[1..-1] {
        if (str:has-prefix $word '-') {
            break
        }
        set command = $command';'$word
    }
    var completions = [
        &'wsblast'= {
            cand -u 'Target WebSocket endpoint flag (alternative to positional argument)'
            cand --url 'Target WebSocket endpoint flag (alternative to positional argument)'
            cand -c 'Number of concurrent WebSocket connection workers'
            cand --connections 'Number of concurrent WebSocket connection workers'
            cand --ramp-rate 'Connection ramp rate in workers per second (0 = spawn all connections immediately)'
            cand -d 'Total test duration including the connection ramp (e.g., ''10s'', ''30s'', ''2m'', ''500ms'')'
            cand --duration 'Total test duration including the connection ramp (e.g., ''10s'', ''30s'', ''2m'', ''500ms'')'
            cand --stages 'Staged load profile instead of -c, -d and --ramp-rate: comma-separated DURATION:CONNECTIONS stages, each moving the connection count linearly from the previous target (starting at 0), e.g. ''30s:100,2m:100,30s:0'''
            cand -n 'Maximum total messages to send across all workers (optional limit)'
            cand --requests 'Maximum total messages to send across all workers (optional limit)'
            cand -r 'Message rate per connection per second (0 = unthrottled / maximum possible)'
            cand --rate 'Message rate per connection per second (0 = unthrottled / maximum possible)'
            cand -p 'Inline message payload to send (supports {{timestamp}}, {{worker_id}}, {{seq}})'
            cand --payload 'Inline message payload to send (supports {{timestamp}}, {{worker_id}}, {{seq}})'
            cand --payload-file 'Path to a file containing the message payload'
            cand -H 'Custom HTTP header to include in the WebSocket handshake (can be specified multiple times)'
            cand --header 'Custom HTTP header to include in the WebSocket handshake (can be specified multiple times)'
            cand --subprotocol 'Optional WebSocket subprotocol to request during handshake'
            cand --mode 'Load testing execution mode'
            cand --connect-timeout 'Connection handshake timeout (e.g., ''5s'', ''2000ms'')'
            cand --message-timeout 'Per-message response timeout in echo mode (e.g., ''5s'', ''1000ms'')'
            cand --match 'Echo reply matching: first (default: the first data frame after a send is the reply), exact, or json:POINTER (an RFC 6901 pointer such as json:/seq)'
            cand --ping-interval 'Periodic ping heartbeat interval (e.g., ''10s'', ''0s'' to disable)'
            cand --max-p50 'SLO threshold: Maximum acceptable p50 round-trip latency (e.g., ''10ms'', ''500us'')'
            cand --max-p95 'SLO threshold: Maximum acceptable p95 round-trip latency (e.g., ''50ms'', ''20ms'')'
            cand --max-p99 'SLO threshold: Maximum acceptable p99 round-trip latency (e.g., ''100ms'', ''50ms'')'
            cand --max-p999 'SLO threshold: Maximum acceptable p99.9 round-trip latency (e.g., ''250ms'')'
            cand --max-error-rate 'SLO threshold: Maximum acceptable error rate fraction (e.g., ''0.01'' for 1%, ''0.001'' for 0.1%)'
            cand --min-throughput 'SLO threshold: Minimum required message throughput (messages/sec)'
            cand --auth-type 'Authentication type (none, static, apikey)'
            cand --auth-token 'Static token for auth (used with --auth-type static)'
            cand --auth-token-cmd 'Command whose trimmed stdout is the token or API key, run through the platform shell before the run (replaces --auth-token or --auth-apikey)'
            cand --auth-token-cmd-timeout 'Time allowed for each run of --auth-token-cmd [default: 30s]'
            cand --auth-token-refresh 'Re-run --auth-token-cmd every DURATION during the run; new handshakes use the latest token'
            cand --auth-header 'Header name for static token (used with --auth-type static)'
            cand --auth-apikey 'API Key (used with --auth-type apikey)'
            cand --auth-apikey-header 'API Key header name (used with --auth-type apikey)'
            cand --graphql-ws-operation-name 'GraphQL WS operation name'
            cand --graphql-ws-operation 'GraphQL WS operation (query/mutation/subscription)'
            cand --graphql-ws-variables 'GraphQL WS variables (JSON)'
            cand --graphql-ws-extensions 'GraphQL WS extensions (JSON)'
            cand --format 'Output report format'
            cand --compare 'Compare two JSON reports for regression detection'
            cand --compare-threshold 'Regression threshold percentage (default: 10%)'
            cand --capture 'Record every data frame and connection event to FILE as JSON lines (see METHODOLOGY.md)'
            cand --capture-limit 'Maximum records --capture writes; later ones are dropped and counted (0 = unlimited)'
            cand -o 'Write output report to the specified file path'
            cand --output 'Write output report to the specified file path'
            cand --prometheus-listen 'Serve live metrics at http://ADDR/metrics in the Prometheus text format while the run lasts (e.g. ''127.0.0.1:9464''; port 0 picks a free port)'
            cand --otel-endpoint 'Export live metrics over OTLP/HTTP with protobuf bodies to this base URL, e.g. ''http://localhost:4318'' (needs the otel feature)'
            cand --otel-interval 'How often metrics are exported over OTLP; the final values are also exported when the run ends'
            cand --otel-service-name 'OpenTelemetry service.name resource attribute [default: $OTEL_SERVICE_NAME, else wsblast]'
            cand --scenario 'Path to scenario YAML file defining multi-phase test (conflicts with individual flags)'
            cand --phase 'Run only a specific phase from the scenario by name'
            cand --binary 'Send payload as WebSocket binary frames instead of text frames'
            cand --fail-on-regression 'Fail on regression (exit code 2)'
            cand --tui 'Launch interactive terminal dashboard (TUI)'
            cand --no-progress 'Suppress interactive progress bar (recommended for non-interactive CI/CD runners)'
            cand --no-reconnect 'End a connection''s worker after a timeout, I/O error or server close instead of reconnecting with backoff'
            cand -h 'Print help (see more with ''--help'')'
            cand --help 'Print help (see more with ''--help'')'
            cand -V 'Print version'
            cand --version 'Print version'
            cand mock 'Run a local WebSocket test server: echo with optional delay, jitter, stalls, rejections and closes, or replay of a capture file'
        }
        &'wsblast;mock'= {
            cand --listen 'Address to listen on (host:port; port 0 picks a free port)'
            cand --delay 'Hold every reply for this long after its request arrived (e.g. ''20ms'')'
            cand --jitter 'Add a uniformly distributed extra delay between 0 and this to every reply'
            cand --close-after 'Close each connection with code 1000 after it sent N replies'
            cand --reject-ratio 'Answer this fraction (0.0 to 1.0) of upgrade requests with HTTP 503'
            cand --stall-every 'Stop replying on every connection at each multiple of this period (needs --stall-for)'
            cand --stall-for 'How long each stall lasts; shorter than --stall-every'
            cand --replay 'Instead of echoing, send each connection the frames a --capture file recorded as received, with their timing'
            cand -h 'Print help'
            cand --help 'Print help'
        }
    ]
    $completions[$command]
}
