# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_wsblast_global_optspecs
    string join \n u/url= c/connections= ramp-rate= d/duration= stages= n/requests= r/rate= p/payload= payload-file= binary H/header= subprotocol= mode= connect-timeout= message-timeout= match= ping-interval= max-p50= max-p95= max-p99= max-p999= max-error-rate= min-throughput= auth-type= auth-token= auth-token-cmd= auth-token-cmd-timeout= auth-token-refresh= auth-header= auth-apikey= auth-apikey-header= graphql-ws-operation-name= graphql-ws-operation= graphql-ws-variables= graphql-ws-extensions= format= compare= compare-threshold= fail-on-regression capture= capture-limit= o/output= tui no-progress no-reconnect prometheus-listen= otel-endpoint= otel-interval= otel-service-name= scenario= phase= agents= agent-token= agent-timeout= agent-ca= h/help V/version
end

function __fish_wsblast_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_wsblast_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_wsblast_using_subcommand
    set -l cmd (__fish_wsblast_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c wsblast -n "__fish_wsblast_needs_command" -s u -l url -d 'Target WebSocket endpoint flag (alternative to positional argument)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -s c -l connections -d 'Number of concurrent WebSocket connection workers' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l ramp-rate -d 'Connection ramp rate in workers per second (0 = spawn all connections immediately)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -s d -l duration -d 'Total test duration including the connection ramp (e.g., \'10s\', \'30s\', \'2m\', \'500ms\')' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l stages -d 'Staged load profile instead of -c, -d and --ramp-rate: comma-separated DURATION:CONNECTIONS stages, each moving the connection count linearly from the previous target (starting at 0), e.g. \'30s:100,2m:100,30s:0\'' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -s n -l requests -d 'Maximum total messages to send across all workers (optional limit)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -s r -l rate -d 'Message rate per connection per second (0 = unthrottled / maximum possible)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -s p -l payload -d 'Inline message payload to send (supports {{timestamp}}, {{worker_id}}, {{seq}})' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l payload-file -d 'Path to a file containing the message payload' -r -F
complete -c wsblast -n "__fish_wsblast_needs_command" -s H -l header -d 'Custom HTTP header to include in the WebSocket handshake (can be specified multiple times)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l subprotocol -d 'Optional WebSocket subprotocol to request during handshake' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l mode -d 'Load testing execution mode' -r -f -a "echo\t'Send request frame, await response, and record round-trip latency (RTT)'
stream\t'Continuously stream frames without awaiting per-frame acknowledgments'
listen\t'Connect workers and passively record incoming server-pushed broadcast messages'
graphql-ws\t'Apollo GraphQL WS protocol (subscriptions)'"
complete -c wsblast -n "__fish_wsblast_needs_command" -l connect-timeout -d 'Connection handshake timeout (e.g., \'5s\', \'2000ms\')' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l message-timeout -d 'Per-message response timeout in echo mode (e.g., \'5s\', \'1000ms\')' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l match -d 'Echo reply matching: first (default: the first data frame after a send is the reply), exact, or json:POINTER (an RFC 6901 pointer such as json:/seq)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l ping-interval -d 'Periodic ping heartbeat interval (e.g., \'10s\', \'0s\' to disable)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l max-p50 -d 'SLO threshold: Maximum acceptable p50 round-trip latency (e.g., \'10ms\', \'500us\')' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l max-p95 -d 'SLO threshold: Maximum acceptable p95 round-trip latency (e.g., \'50ms\', \'20ms\')' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l max-p99 -d 'SLO threshold: Maximum acceptable p99 round-trip latency (e.g., \'100ms\', \'50ms\')' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l max-p999 -d 'SLO threshold: Maximum acceptable p99.9 round-trip latency (e.g., \'250ms\')' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l max-error-rate -d 'SLO threshold: Maximum acceptable error rate fraction (e.g., \'0.01\' for 1%, \'0.001\' for 0.1%)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l min-throughput -d 'SLO threshold: Minimum required message throughput (messages/sec)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l auth-type -d 'Authentication type (none, static, apikey)' -r -f -a "none\t'No authentication'
static\t'Static bearer token'
apikey\t'API Key'"
complete -c wsblast -n "__fish_wsblast_needs_command" -l auth-token -d 'Static token for auth (used with --auth-type static)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l auth-token-cmd -d 'Command whose trimmed stdout is the token or API key, run through the platform shell before the run (replaces --auth-token or --auth-apikey)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l auth-token-cmd-timeout -d 'Time allowed for each run of --auth-token-cmd [default: 30s]' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l auth-token-refresh -d 'Re-run --auth-token-cmd every DURATION during the run; new handshakes use the latest token' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l auth-header -d 'Header name for static token (used with --auth-type static)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l auth-apikey -d 'API Key (used with --auth-type apikey)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l auth-apikey-header -d 'API Key header name (used with --auth-type apikey)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l graphql-ws-operation-name -d 'GraphQL WS operation name' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l graphql-ws-operation -d 'GraphQL WS operation (query/mutation/subscription)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l graphql-ws-variables -d 'GraphQL WS variables (JSON)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l graphql-ws-extensions -d 'GraphQL WS extensions (JSON)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l format -d 'Output report format' -r -f -a "text\t'Human-readable styled terminal tables'
json\t'Schema-versioned JSON for CI parsing and metric archiving'
markdown\t'GitHub Flavored Markdown table for PR comments and job summaries'
html\t'A self-contained HTML page with charts of the time series, to open in a browser or attach to a CI run'"
complete -c wsblast -n "__fish_wsblast_needs_command" -l compare -d 'Compare two JSON reports for regression detection' -r -F
complete -c wsblast -n "__fish_wsblast_needs_command" -l compare-threshold -d 'Regression threshold percentage (default: 10%)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l capture -d 'Record every data frame and connection event to FILE as JSON lines (see METHODOLOGY.md)' -r -F
complete -c wsblast -n "__fish_wsblast_needs_command" -l capture-limit -d 'Maximum records --capture writes; later ones are dropped and counted (0 = unlimited)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -s o -l output -d 'Write output report to the specified file path' -r -F
complete -c wsblast -n "__fish_wsblast_needs_command" -l prometheus-listen -d 'Serve live metrics at http://ADDR/metrics in the Prometheus text format while the run lasts (e.g. \'127.0.0.1:9464\'; port 0 picks a free port)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l otel-endpoint -d 'Export live metrics over OTLP/HTTP with protobuf bodies to this base URL, e.g. \'http://localhost:4318\' (needs the otel feature)' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l otel-interval -d 'How often metrics are exported over OTLP; the final values are also exported when the run ends' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l otel-service-name -d 'OpenTelemetry service.name resource attribute [default: $OTEL_SERVICE_NAME, else wsblast]' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l scenario -d 'Path to scenario YAML file defining multi-phase test (conflicts with individual flags)' -r -F
complete -c wsblast -n "__fish_wsblast_needs_command" -l phase -d 'Run only a specific phase from the scenario by name' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l agents -d 'Generate the load on these agents (started with `wsblast agent`) instead of locally; comma-separated HOST:PORT, ws://HOST:PORT or wss://HOST:PORT' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l agent-token -d 'Token the agents were started with [default: $WSBLAST_AGENT_TOKEN]' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l agent-timeout -d 'Time the agents get to become ready before the start, and to send their results after the end of the run' -r
complete -c wsblast -n "__fish_wsblast_needs_command" -l agent-ca -d 'PEM file of CA certificates to trust for wss:// agents, in addition to the platform\'s roots' -r -F
complete -c wsblast -n "__fish_wsblast_needs_command" -l binary -d 'Send payload as WebSocket binary frames instead of text frames'
complete -c wsblast -n "__fish_wsblast_needs_command" -l fail-on-regression -d 'Fail on regression (exit code 2)'
complete -c wsblast -n "__fish_wsblast_needs_command" -l tui -d 'Launch interactive terminal dashboard (TUI)'
complete -c wsblast -n "__fish_wsblast_needs_command" -l no-progress -d 'Suppress interactive progress bar (recommended for non-interactive CI/CD runners)'
complete -c wsblast -n "__fish_wsblast_needs_command" -l no-reconnect -d 'End a connection\'s worker after a timeout, I/O error or server close instead of reconnecting with backoff'
complete -c wsblast -n "__fish_wsblast_needs_command" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c wsblast -n "__fish_wsblast_needs_command" -s V -l version -d 'Print version'
complete -c wsblast -n "__fish_wsblast_needs_command" -a "mock" -d 'Run a local WebSocket test server: echo with optional delay, jitter, stalls, rejections and closes, or replay of a capture file'
complete -c wsblast -n "__fish_wsblast_needs_command" -a "agent" -d 'Run a load agent that generates load for a coordinator (`wsblast URL --agents ...`), one test at a time'
complete -c wsblast -n "__fish_wsblast_using_subcommand mock" -l listen -d 'Address to listen on (host:port; port 0 picks a free port)' -r
complete -c wsblast -n "__fish_wsblast_using_subcommand mock" -l delay -d 'Hold every reply for this long after its request arrived (e.g. \'20ms\')' -r
complete -c wsblast -n "__fish_wsblast_using_subcommand mock" -l jitter -d 'Add a uniformly distributed extra delay between 0 and this to every reply' -r
complete -c wsblast -n "__fish_wsblast_using_subcommand mock" -l close-after -d 'Close each connection with code 1000 after it sent N replies' -r
complete -c wsblast -n "__fish_wsblast_using_subcommand mock" -l reject-ratio -d 'Answer this fraction (0.0 to 1.0) of upgrade requests with HTTP 503' -r
complete -c wsblast -n "__fish_wsblast_using_subcommand mock" -l stall-every -d 'Stop replying on every connection at each multiple of this period (needs --stall-for)' -r
complete -c wsblast -n "__fish_wsblast_using_subcommand mock" -l stall-for -d 'How long each stall lasts; shorter than --stall-every' -r
complete -c wsblast -n "__fish_wsblast_using_subcommand mock" -l replay -d 'Instead of echoing, send each connection the frames a --capture file recorded as received, with their timing' -r -F
complete -c wsblast -n "__fish_wsblast_using_subcommand mock" -s h -l help -d 'Print help'
complete -c wsblast -n "__fish_wsblast_using_subcommand agent" -l listen -d 'Address to accept coordinators on (host:port, e.g. \'0.0.0.0:7700\'; port 0 picks a free port)' -r
complete -c wsblast -n "__fish_wsblast_using_subcommand agent" -l token -d 'Token coordinators must present, at least 16 characters; required, here or in WSBLAST_AGENT_TOKEN' -r
complete -c wsblast -n "__fish_wsblast_using_subcommand agent" -l tls-cert -d 'PEM certificate chain to serve the control channel over TLS (wss://)' -r -F
complete -c wsblast -n "__fish_wsblast_using_subcommand agent" -l tls-key -d 'PEM private key for --tls-cert' -r -F
complete -c wsblast -n "__fish_wsblast_using_subcommand agent" -s h -l help -d 'Print help'
