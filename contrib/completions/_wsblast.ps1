
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
            [CompletionResult]::new('--auth-type', '--auth-type', [CompletionResultType]::ParameterName, 'Authentication type (none, static, apikey)')
            [CompletionResult]::new('--auth-token', '--auth-token', [CompletionResultType]::ParameterName, 'Static token for auth (used with --auth-type static)')
            [CompletionResult]::new('--auth-header', '--auth-header', [CompletionResultType]::ParameterName, 'Header name for static token (used with --auth-type static)')
            [CompletionResult]::new('--auth-apikey', '--auth-apikey', [CompletionResultType]::ParameterName, 'API Key (used with --auth-type apikey)')
            [CompletionResult]::new('--auth-apikey-header', '--auth-apikey-header', [CompletionResultType]::ParameterName, 'API Key header name (used with --auth-type apikey)')
            [CompletionResult]::new('--graphql-ws-operation-name', '--graphql-ws-operation-name', [CompletionResultType]::ParameterName, 'GraphQL WS operation name')
            [CompletionResult]::new('--graphql-ws-operation', '--graphql-ws-operation', [CompletionResultType]::ParameterName, 'GraphQL WS operation (query/mutation/subscription)')
            [CompletionResult]::new('--graphql-ws-variables', '--graphql-ws-variables', [CompletionResultType]::ParameterName, 'GraphQL WS variables (JSON)')
            [CompletionResult]::new('--graphql-ws-extensions', '--graphql-ws-extensions', [CompletionResultType]::ParameterName, 'GraphQL WS extensions (JSON)')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Output report format')
            [CompletionResult]::new('--compare', '--compare', [CompletionResultType]::ParameterName, 'Compare two JSON reports for regression detection')
            [CompletionResult]::new('--compare-threshold', '--compare-threshold', [CompletionResultType]::ParameterName, 'Regression threshold percentage (default: 10%)')
            [CompletionResult]::new('-o', '-o', [CompletionResultType]::ParameterName, 'Write output report to the specified file path')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Write output report to the specified file path')
            [CompletionResult]::new('--scenario', '--scenario', [CompletionResultType]::ParameterName, 'Path to scenario YAML file defining multi-phase test (conflicts with individual flags)')
            [CompletionResult]::new('--phase', '--phase', [CompletionResultType]::ParameterName, 'Run only a specific phase from the scenario by name')
            [CompletionResult]::new('--binary', '--binary', [CompletionResultType]::ParameterName, 'Send payload as WebSocket binary frames instead of text frames')
            [CompletionResult]::new('--fail-on-regression', '--fail-on-regression', [CompletionResultType]::ParameterName, 'Fail on regression (exit code 2)')
            [CompletionResult]::new('--tui', '--tui', [CompletionResultType]::ParameterName, 'Launch interactive terminal dashboard (TUI)')
            [CompletionResult]::new('--no-progress', '--no-progress', [CompletionResultType]::ParameterName, 'Suppress interactive progress bar (recommended for non-interactive CI/CD runners)')
            [CompletionResult]::new('--no-reconnect', '--no-reconnect', [CompletionResultType]::ParameterName, 'End a connection''s worker after a timeout, I/O error or server close instead of reconnecting with backoff')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}
