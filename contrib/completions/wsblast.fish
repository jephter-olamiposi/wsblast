complete -c wsblast -s u -l url -d 'Target WebSocket endpoint flag (alternative to positional argument)' -r
complete -c wsblast -s c -l connections -d 'Number of concurrent WebSocket connection workers' -r
complete -c wsblast -l ramp-rate -d 'Connection ramp rate in workers per second (0 = spawn all connections immediately)' -r
complete -c wsblast -s d -l duration -d 'Total test duration including the connection ramp (e.g., \'10s\', \'30s\', \'2m\', \'500ms\')' -r
complete -c wsblast -s n -l requests -d 'Maximum total messages to send across all workers (optional limit)' -r
complete -c wsblast -s r -l rate -d 'Message rate per connection per second (0 = unthrottled / maximum possible)' -r
complete -c wsblast -s p -l payload -d 'Inline message payload to send (supports {{timestamp}}, {{worker_id}}, {{seq}})' -r
complete -c wsblast -l payload-file -d 'Path to a file containing the message payload' -r -F
complete -c wsblast -s H -l header -d 'Custom HTTP header to include in the WebSocket handshake (can be specified multiple times)' -r
complete -c wsblast -l subprotocol -d 'Optional WebSocket subprotocol to request during handshake' -r
complete -c wsblast -l mode -d 'Load testing execution mode' -r -f -a "echo\t'Send request frame, await response, and record round-trip latency (RTT)'
stream\t'Continuously stream frames without awaiting per-frame acknowledgments'
listen\t'Connect workers and passively record incoming server-pushed broadcast messages'
graphql-ws\t'Apollo GraphQL WS protocol (subscriptions)'"
complete -c wsblast -l connect-timeout -d 'Connection handshake timeout (e.g., \'5s\', \'2000ms\')' -r
complete -c wsblast -l message-timeout -d 'Per-message response timeout in echo mode (e.g., \'5s\', \'1000ms\')' -r
complete -c wsblast -l match -d 'Echo reply matching: first (default: the first data frame after a send is the reply), exact, or json:POINTER (an RFC 6901 pointer such as json:/seq)' -r
complete -c wsblast -l ping-interval -d 'Periodic ping heartbeat interval (e.g., \'10s\', \'0s\' to disable)' -r
complete -c wsblast -l max-p50 -d 'SLO threshold: Maximum acceptable p50 round-trip latency (e.g., \'10ms\', \'500us\')' -r
complete -c wsblast -l max-p95 -d 'SLO threshold: Maximum acceptable p95 round-trip latency (e.g., \'50ms\', \'20ms\')' -r
complete -c wsblast -l max-p99 -d 'SLO threshold: Maximum acceptable p99 round-trip latency (e.g., \'100ms\', \'50ms\')' -r
complete -c wsblast -l max-p999 -d 'SLO threshold: Maximum acceptable p99.9 round-trip latency (e.g., \'250ms\')' -r
complete -c wsblast -l max-error-rate -d 'SLO threshold: Maximum acceptable error rate fraction (e.g., \'0.01\' for 1%, \'0.001\' for 0.1%)' -r
complete -c wsblast -l min-throughput -d 'SLO threshold: Minimum required message throughput (messages/sec)' -r
complete -c wsblast -l auth-type -d 'Authentication type (none, static, apikey)' -r -f -a "none\t'No authentication'
static\t'Static bearer token'
apikey\t'API Key'"
complete -c wsblast -l auth-token -d 'Static token for auth (used with --auth-type static)' -r
complete -c wsblast -l auth-header -d 'Header name for static token (used with --auth-type static)' -r
complete -c wsblast -l auth-apikey -d 'API Key (used with --auth-type apikey)' -r
complete -c wsblast -l auth-apikey-header -d 'API Key header name (used with --auth-type apikey)' -r
complete -c wsblast -l graphql-ws-operation-name -d 'GraphQL WS operation name' -r
complete -c wsblast -l graphql-ws-operation -d 'GraphQL WS operation (query/mutation/subscription)' -r
complete -c wsblast -l graphql-ws-variables -d 'GraphQL WS variables (JSON)' -r
complete -c wsblast -l graphql-ws-extensions -d 'GraphQL WS extensions (JSON)' -r
complete -c wsblast -l format -d 'Output report format' -r -f -a "text\t'Human-readable styled terminal tables'
json\t'Schema-versioned JSON for CI parsing and metric archiving'
markdown\t'GitHub Flavored Markdown table for PR comments and job summaries'"
complete -c wsblast -l compare -d 'Compare two JSON reports for regression detection' -r -F
complete -c wsblast -l compare-threshold -d 'Regression threshold percentage (default: 10%)' -r
complete -c wsblast -s o -l output -d 'Write output report to the specified file path' -r -F
complete -c wsblast -l scenario -d 'Path to scenario YAML file defining multi-phase test (conflicts with individual flags)' -r -F
complete -c wsblast -l phase -d 'Run only a specific phase from the scenario by name' -r
complete -c wsblast -l binary -d 'Send payload as WebSocket binary frames instead of text frames'
complete -c wsblast -l fail-on-regression -d 'Fail on regression (exit code 2)'
complete -c wsblast -l tui -d 'Launch interactive terminal dashboard (TUI)'
complete -c wsblast -l no-progress -d 'Suppress interactive progress bar (recommended for non-interactive CI/CD runners)'
complete -c wsblast -l no-reconnect -d 'End a connection\'s worker after a timeout, I/O error or server close instead of reconnecting with backoff'
complete -c wsblast -s h -l help -d 'Print help (see more with \'--help\')'
complete -c wsblast -s V -l version -d 'Print version'
