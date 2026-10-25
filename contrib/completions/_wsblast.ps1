
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'wsblast' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'wsblast'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'wsblast' {
            [CompletionResult]::new('-u', '-u', [CompletionResultType]::ParameterName, 'Target WebSocket endpoint flag (alternative to positional argument)')
            [CompletionResult]::new('--url', '--url', [CompletionResultType]::ParameterName, 'Target WebSocket endpoint flag (alternative to positional argument)')
            [CompletionResult]::new('-c', '-c', [CompletionResultType]::ParameterName, 'Number of concurrent WebSocket connection workers')
            [CompletionResult]::new('--connections', '--connections', [CompletionResultType]::ParameterName, 'Number of concurrent WebSocket connection workers')
            [CompletionResult]::new('--ramp-rate', '--ramp-rate', [CompletionResultType]::ParameterName, 'Connection ramp rate in workers per second (0 = spawn all connections immediately)')
            [CompletionResult]::new('-d', '-d', [CompletionResultType]::ParameterName, 'Total test duration including the connection ramp (e.g., ''10s'', ''30s'', ''2m'', ''500ms'')')
            [CompletionResult]::new('--duration', '--duration', [CompletionResultType]::ParameterName, 'Total test duration including the connection ramp (e.g., ''10s'', ''30s'', ''2m'', ''500ms'')')
            [CompletionResult]::new('--stages', '--stages', [CompletionResultType]::ParameterName, 'Staged load profile instead of -c, -d and --ramp-rate: comma-separated DURATION:CONNECTIONS stages, each moving the connection count linearly from the previous target (starting at 0), e.g. ''30s:100,2m:100,30s:0''')
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Maximum total messages to send across all workers (optional limit)')
            [CompletionResult]::new('--requests', '--requests', [CompletionResultType]::ParameterName, 'Maximum total messages to send across all workers (optional limit)')
            [CompletionResult]::new('-r', '-r', [CompletionResultType]::ParameterName, 'Message rate per connection per second (0 = unthrottled / maximum possible)')
            [CompletionResult]::new('--rate', '--rate', [CompletionResultType]::ParameterName, 'Message rate per connection per second (0 = unthrottled / maximum possible)')
            [CompletionResult]::new('-p', '-p', [CompletionResultType]::ParameterName, 'Inline message payload to send (supports {{timestamp}}, {{worker_id}}, {{seq}})')
            [CompletionResult]::new('--payload', '--payload', [CompletionResultType]::ParameterName, 'Inline message payload to send (supports {{timestamp}}, {{worker_id}}, {{seq}})')
            [CompletionResult]::new('--payload-file', '--payload-file', [CompletionResultType]::ParameterName, 'Path to a file containing the message payload')
            [CompletionResult]::new('-H', '-H ', [CompletionResultType]::ParameterName, 'Custom HTTP header to include in the WebSocket handshake (can be specified multiple times)')
            [CompletionResult]::new('--header', '--header', [CompletionResultType]::ParameterName, 'Custom HTTP header to include in the WebSocket handshake (can be specified multiple times)')
            [CompletionResult]::new('--subprotocol', '--subprotocol', [CompletionResultType]::ParameterName, 'Optional WebSocket subprotocol to request during handshake')
            [CompletionResult]::new('--mode', '--mode', [CompletionResultType]::ParameterName, 'Load testing execution mode')
            [CompletionResult]::new('--connect-timeout', '--connect-timeout', [CompletionResultType]::ParameterName, 'Connection handshake timeout (e.g., ''5s'', ''2000ms'')')
            [CompletionResult]::new('--message-timeout', '--message-timeout', [CompletionResultType]::ParameterName, 'Per-message response timeout in echo mode (e.g., ''5s'', ''1000ms'')')
            [CompletionResult]::new('--match', '--match', [CompletionResultType]::ParameterName, 'Echo reply matching: first (default: the first data frame after a send is the reply), exact, or json:POINTER (an RFC 6901 pointer such as json:/seq)')
            [CompletionResult]::new('--ping-interval', '--ping-interval', [CompletionResultType]::ParameterName, 'Periodic ping heartbeat interval (e.g., ''10s'', ''0s'' to disable)')
            [CompletionResult]::new('--max-p50', '--max-p50', [CompletionResultType]::ParameterName, 'SLO threshold: Maximum acceptable p50 round-trip latency (e.g., ''10ms'', ''500us'')')
            [CompletionResult]::new('--max-p95', '--max-p95', [CompletionResultType]::ParameterName, 'SLO threshold: Maximum acceptable p95 round-trip latency (e.g., ''50ms'', ''20ms'')')
            [CompletionResult]::new('--max-p99', '--max-p99', [CompletionResultType]::ParameterName, 'SLO threshold: Maximum acceptable p99 round-trip latency (e.g., ''100ms'', ''50ms'')')
            [CompletionResult]::new('--max-p999', '--max-p999', [CompletionResultType]::ParameterName, 'SLO threshold: Maximum acceptable p99.9 round-trip latency (e.g., ''250ms'')')
            [CompletionResult]::new('--max-error-rate', '--max-error-rate', [CompletionResultType]::ParameterName, 'SLO threshold: Maximum acceptable error rate fraction (e.g., ''0.01'' for 1%, ''0.001'' for 0.1%)')
            [CompletionResult]::new('--min-throughput', '--min-throughput', [CompletionResultType]::ParameterName, 'SLO threshold: Minimum required message throughput (messages/sec)')
            [CompletionResult]::new('--max-generator-lag', '--max-generator-lag', [CompletionResultType]::ParameterName, 'SLO threshold: Maximum p99 wake-up lag of the load generator itself (e.g. ''20ms''); a breach means the latency numbers include client-side delay')
            [CompletionResult]::new('--auth-type', '--auth-type', [CompletionResultType]::ParameterName, 'Authentication type')
            [CompletionResult]::new('--auth-token', '--auth-token', [CompletionResultType]::ParameterName, 'Static token for auth (used with --auth-type static)')
            [CompletionResult]::new('--auth-token-cmd', '--auth-token-cmd', [CompletionResultType]::ParameterName, 'Command whose trimmed stdout is the token or API key, run through the platform shell before the run (replaces --auth-token or --auth-apikey)')
            [CompletionResult]::new('--auth-token-cmd-timeout', '--auth-token-cmd-timeout', [CompletionResultType]::ParameterName, 'Time allowed for each run of --auth-token-cmd [default: 30s]')
            [CompletionResult]::new('--auth-token-refresh', '--auth-token-refresh', [CompletionResultType]::ParameterName, 'Re-run --auth-token-cmd every DURATION during the run; new handshakes use the latest token')
            [CompletionResult]::new('--auth-header', '--auth-header', [CompletionResultType]::ParameterName, 'Header that carries the token (used with every --auth-type except apikey)')
            [CompletionResult]::new('--auth-apikey', '--auth-apikey', [CompletionResultType]::ParameterName, 'API Key (used with --auth-type apikey)')
            [CompletionResult]::new('--auth-apikey-header', '--auth-apikey-header', [CompletionResultType]::ParameterName, 'API Key header name (used with --auth-type apikey)')
            [CompletionResult]::new('--auth-jwt-alg', '--auth-jwt-alg', [CompletionResultType]::ParameterName, 'JWT signing algorithm (used with --auth-type jwt) [default: HS256]')
            [CompletionResult]::new('--auth-jwt-key', '--auth-jwt-key', [CompletionResultType]::ParameterName, 'File with the JWT signing key: the secret bytes for HS*, a PEM private key for RS256 and ES256; or set WSBLAST_JWT_KEY_FILE')
            [CompletionResult]::new('--auth-jwt-claims', '--auth-jwt-claims', [CompletionResultType]::ParameterName, 'JSON object of claims added to every JWT (e.g. ''{"sub":"loadtest","aud":"ws"}'')')
            [CompletionResult]::new('--auth-jwt-ttl', '--auth-jwt-ttl', [CompletionResultType]::ParameterName, 'JWT lifetime, which sets iat, nbf and exp; tokens are re-signed before they expire [default: 5m]')
            [CompletionResult]::new('--auth-oauth2-token-url', '--auth-oauth2-token-url', [CompletionResultType]::ParameterName, 'OAuth2 token endpoint for the client-credentials grant (used with --auth-type oauth2)')
            [CompletionResult]::new('--auth-oauth2-client-id', '--auth-oauth2-client-id', [CompletionResultType]::ParameterName, 'OAuth2 client id (used with --auth-type oauth2)')
            [CompletionResult]::new('--auth-oauth2-client-secret-file', '--auth-oauth2-client-secret-file', [CompletionResultType]::ParameterName, 'File with the OAuth2 client secret; or set WSBLAST_OAUTH2_CLIENT_SECRET')
            [CompletionResult]::new('--auth-oauth2-scope', '--auth-oauth2-scope', [CompletionResultType]::ParameterName, 'Scope requested with the OAuth2 token (space-separated)')
            [CompletionResult]::new('--auth-oauth2-audience', '--auth-oauth2-audience', [CompletionResultType]::ParameterName, 'Audience parameter sent with the OAuth2 token request, for servers that require one')
            [CompletionResult]::new('--graphql-ws-operation-name', '--graphql-ws-operation-name', [CompletionResultType]::ParameterName, 'GraphQL WS operation name')
            [CompletionResult]::new('--graphql-ws-operation', '--graphql-ws-operation', [CompletionResultType]::ParameterName, 'GraphQL WS operation (query/mutation/subscription)')
            [CompletionResult]::new('--graphql-ws-variables', '--graphql-ws-variables', [CompletionResultType]::ParameterName, 'GraphQL WS variables (JSON)')
            [CompletionResult]::new('--graphql-ws-extensions', '--graphql-ws-extensions', [CompletionResultType]::ParameterName, 'GraphQL WS extensions (JSON)')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Output report format')
            [CompletionResult]::new('--compare', '--compare', [CompletionResultType]::ParameterName, 'Compare two JSON reports for regression detection (--format text, json, markdown or html)')
            [CompletionResult]::new('--compare-threshold', '--compare-threshold', [CompletionResultType]::ParameterName, 'Regression threshold percentage (default: 10%)')
            [CompletionResult]::new('--compare-window', '--compare-window', [CompletionResultType]::ParameterName, 'Restrict the time-series metrics of --compare to START..END of each run (e.g., ''10s..50s'', ''10s..'', ''..50s'')')
            [CompletionResult]::new('--capture', '--capture', [CompletionResultType]::ParameterName, 'Record every data frame and connection event to FILE as JSON lines (see METHODOLOGY.md)')
            [CompletionResult]::new('--capture-limit', '--capture-limit', [CompletionResultType]::ParameterName, 'Maximum records --capture writes; later ones are dropped and counted (0 = unlimited)')
            [CompletionResult]::new('--chaos-delay', '--chaos-delay', [CompletionResultType]::ParameterName, 'Chaos: pause DURATION, plus up to JITTER drawn uniformly, before every data send (e.g. ''5ms'' or ''5ms:20ms'')')
            [CompletionResult]::new('--chaos-drop', '--chaos-drop', [CompletionResultType]::ParameterName, 'Chaos: skip this percentage (0 to 100) of data sends; they are counted as dropped, not as sent')
            [CompletionResult]::new('--chaos-corrupt', '--chaos-corrupt', [CompletionResultType]::ParameterName, 'Chaos: change one byte of the payload of this percentage (0 to 100) of data sends; echo replies are still matched and timed')
            [CompletionResult]::new('--chaos-close', '--chaos-close', [CompletionResultType]::ParameterName, 'Chaos: close the connection after this percentage (0 to 100) of replies, or of sends in stream mode; the worker reconnects, and closes are not errors')
            [CompletionResult]::new('--chaos-close-mode', '--chaos-close-mode', [CompletionResultType]::ParameterName, 'How --chaos-close ends a connection')
            [CompletionResult]::new('--chaos-seed', '--chaos-seed', [CompletionResultType]::ParameterName, 'Chaos: seed of every chaos decision, to repeat a run''s faults [default: picked per run and shown in the report]')
            [CompletionResult]::new('-o', '-o', [CompletionResultType]::ParameterName, 'Write output report to the specified file path')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Write output report to the specified file path')
            [CompletionResult]::new('--prometheus-listen', '--prometheus-listen', [CompletionResultType]::ParameterName, 'Serve live metrics at http://ADDR/metrics in the Prometheus text format while the run lasts (e.g. ''127.0.0.1:9464''; port 0 picks a free port)')
            [CompletionResult]::new('--otel-endpoint', '--otel-endpoint', [CompletionResultType]::ParameterName, 'Export live metrics over OTLP/HTTP with protobuf bodies to this base URL, e.g. ''http://localhost:4318'' (needs the otel feature)')
            [CompletionResult]::new('--otel-interval', '--otel-interval', [CompletionResultType]::ParameterName, 'How often metrics are exported over OTLP; the final values are also exported when the run ends')
            [CompletionResult]::new('--otel-service-name', '--otel-service-name', [CompletionResultType]::ParameterName, 'OpenTelemetry service.name resource attribute [default: $OTEL_SERVICE_NAME, else wsblast]')
            [CompletionResult]::new('--scenario', '--scenario', [CompletionResultType]::ParameterName, 'Path to scenario YAML file defining multi-phase test (conflicts with individual flags)')
            [CompletionResult]::new('--phase', '--phase', [CompletionResultType]::ParameterName, 'Run only a specific phase from the scenario by name')
            [CompletionResult]::new('--agents', '--agents', [CompletionResultType]::ParameterName, 'Generate the load on these agents (started with `wsblast agent`) instead of locally; comma-separated HOST:PORT, ws://HOST:PORT or wss://HOST:PORT')
            [CompletionResult]::new('--agent-token', '--agent-token', [CompletionResultType]::ParameterName, 'Token the agents were started with [default: $WSBLAST_AGENT_TOKEN]')
            [CompletionResult]::new('--agent-timeout', '--agent-timeout', [CompletionResultType]::ParameterName, 'Time the agents get to become ready before the start, and to send their results after the end of the run')
            [CompletionResult]::new('--agent-ca', '--agent-ca', [CompletionResultType]::ParameterName, 'PEM file of CA certificates to trust for wss:// agents, in addition to the platform''s roots')
            [CompletionResult]::new('--binary', '--binary', [CompletionResultType]::ParameterName, 'Send payload as WebSocket binary frames instead of text frames')
            [CompletionResult]::new('--auth-jwt-per-connection', '--auth-jwt-per-connection', [CompletionResultType]::ParameterName, 'Sign a fresh JWT with a unique jti for every connection attempt instead of sharing one token')
            [CompletionResult]::new('--fail-on-regression', '--fail-on-regression', [CompletionResultType]::ParameterName, 'Fail on regression (exit code 2)')
            [CompletionResult]::new('--tui', '--tui', [CompletionResultType]::ParameterName, 'Launch interactive terminal dashboard (TUI)')
            [CompletionResult]::new('--no-progress', '--no-progress', [CompletionResultType]::ParameterName, 'Suppress interactive progress bar (recommended for non-interactive CI/CD runners)')
            [CompletionResult]::new('--no-reconnect', '--no-reconnect', [CompletionResultType]::ParameterName, 'End a connection''s worker after a timeout, I/O error or server close instead of reconnecting with backoff')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('mock', 'mock', [CompletionResultType]::ParameterValue, 'Run a local WebSocket test server: echo with optional delay, jitter, stalls, rejections and closes, or replay of a capture file')
            [CompletionResult]::new('agent', 'agent', [CompletionResultType]::ParameterValue, 'Run a load agent that generates load for a coordinator (`wsblast URL --agents ...`), one test at a time')
            [CompletionResult]::new('render', 'render', [CompletionResultType]::ParameterValue, 'Render a saved JSON report (from --format json) or comparison (from --compare --format json) as HTML, Markdown or text without running a test')
            break
        }
        'wsblast;mock' {
            [CompletionResult]::new('--listen', '--listen', [CompletionResultType]::ParameterName, 'Address to listen on (host:port; port 0 picks a free port)')
            [CompletionResult]::new('--delay', '--delay', [CompletionResultType]::ParameterName, 'Hold every reply for this long after its request arrived (e.g. ''20ms'')')
            [CompletionResult]::new('--jitter', '--jitter', [CompletionResultType]::ParameterName, 'Add a uniformly distributed extra delay between 0 and this to every reply')
            [CompletionResult]::new('--close-after', '--close-after', [CompletionResultType]::ParameterName, 'Close each connection with code 1000 after it sent N replies')
            [CompletionResult]::new('--reject-ratio', '--reject-ratio', [CompletionResultType]::ParameterName, 'Answer this fraction (0.0 to 1.0) of upgrade requests with HTTP 503')
            [CompletionResult]::new('--stall-every', '--stall-every', [CompletionResultType]::ParameterName, 'Stop replying on every connection at each multiple of this period (needs --stall-for)')
            [CompletionResult]::new('--stall-for', '--stall-for', [CompletionResultType]::ParameterName, 'How long each stall lasts; shorter than --stall-every')
            [CompletionResult]::new('--replay', '--replay', [CompletionResultType]::ParameterName, 'Instead of echoing, send each connection the frames a --capture file recorded as received, with their timing')
            [CompletionResult]::new('--tls-cert', '--tls-cert', [CompletionResultType]::ParameterName, 'PEM certificate chain to serve wss:// instead of ws:// (clients must trust it, e.g. with SSL_CERT_FILE)')
            [CompletionResult]::new('--tls-key', '--tls-key', [CompletionResultType]::ParameterName, 'PEM private key for --tls-cert')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'wsblast;agent' {
            [CompletionResult]::new('--listen', '--listen', [CompletionResultType]::ParameterName, 'Address to accept coordinators on (host:port, e.g. ''0.0.0.0:7700''; port 0 picks a free port)')
            [CompletionResult]::new('--token', '--token', [CompletionResultType]::ParameterName, 'Token coordinators must present, at least 16 characters; required, here or in WSBLAST_AGENT_TOKEN')
            [CompletionResult]::new('--tls-cert', '--tls-cert', [CompletionResultType]::ParameterName, 'PEM certificate chain to serve the control channel over TLS (wss://)')
            [CompletionResult]::new('--tls-key', '--tls-key', [CompletionResultType]::ParameterName, 'PEM private key for --tls-cert')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'wsblast;render' {
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Format to render')
            [CompletionResult]::new('-o', '-o', [CompletionResultType]::ParameterName, 'Write the rendered report to FILE instead of stdout')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Write the rendered report to FILE instead of stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}
