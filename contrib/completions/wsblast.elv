
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
            cand -o 'Write output report to the specified file path'
            cand --output 'Write output report to the specified file path'
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
        }
    ]
    $completions[$command]
}
