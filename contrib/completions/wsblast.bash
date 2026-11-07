_wsblast() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="wsblast"
                ;;
            wsblast,agent)
                cmd="wsblast__subcmd__agent"
                ;;
            wsblast,mock)
                cmd="wsblast__subcmd__mock"
                ;;
            wsblast,render)
                cmd="wsblast__subcmd__render"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        wsblast)
            opts="-u -c -d -n -r -p -H -o -h -V --url --connections --ramp-rate --duration --stages --arrival-rate --session-duration --max-connections --requests --rate --payload --payload-file --binary --data --header --subprotocol --source-ip --mode --connect-timeout --message-timeout --match --expect --ping-interval --max-p50 --max-p95 --max-p99 --max-p999 --max-error-rate --min-throughput --max-generator-lag --auth-type --auth-token --auth-token-cmd --auth-token-cmd-timeout --auth-token-refresh --auth-header --auth-apikey --auth-apikey-header --auth-jwt-alg --auth-jwt-key --auth-jwt-claims --auth-jwt-ttl --auth-jwt-kid --auth-jwt-per-connection --auth-oauth2-token-url --auth-oauth2-client-id --auth-oauth2-client-secret-file --auth-oauth2-scope --auth-oauth2-audience --graphql-ws-operation-name --graphql-ws-operation --graphql-ws-variables --graphql-ws-extensions --sio-event --sio-namespace --sio-auth --format --compare --compare-threshold --compare-window --fail-on-regression --capture --capture-limit --chaos-delay --chaos-drop --chaos-corrupt --chaos-close --chaos-close-mode --chaos-read-pause --chaos-seed --output --tui --no-progress --no-reconnect --prometheus-listen --otel-endpoint --otel-interval --otel-service-name --scenario --phase --agents --agent-token --agent-timeout --agent-ca --help --version mock agent render"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --url)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -u)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --connections)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --ramp-rate)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --duration)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -d)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --stages)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --arrival-rate)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-duration)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-connections)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --requests)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -n)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --rate)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -r)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --payload)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -p)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --payload-file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --data)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --header)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -H)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --subprotocol)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --source-ip)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mode)
                    COMPREPLY=($(compgen -W "echo stream listen graphql-ws socketio" -- "${cur}"))
                    return 0
                    ;;
                --connect-timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --message-timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --match)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --expect)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --ping-interval)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-p50)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-p95)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-p99)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-p999)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-error-rate)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --min-throughput)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-generator-lag)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-type)
                    COMPREPLY=($(compgen -W "none static apikey jwt oauth2" -- "${cur}"))
                    return 0
                    ;;
                --auth-token)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-token-cmd)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-token-cmd-timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-token-refresh)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-header)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-apikey)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-apikey-header)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-jwt-alg)
                    COMPREPLY=($(compgen -W "HS256 HS384 HS512 RS256 ES256" -- "${cur}"))
                    return 0
                    ;;
                --auth-jwt-key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-jwt-claims)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-jwt-ttl)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-jwt-kid)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-oauth2-token-url)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-oauth2-client-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-oauth2-client-secret-file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-oauth2-scope)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --auth-oauth2-audience)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --graphql-ws-operation-name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --graphql-ws-operation)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --graphql-ws-variables)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --graphql-ws-extensions)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --sio-event)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --sio-namespace)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --sio-auth)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --format)
                    COMPREPLY=($(compgen -W "text json markdown html" -- "${cur}"))
                    return 0
                    ;;
                --compare)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --compare-threshold)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --compare-window)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --capture)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --capture-limit)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chaos-delay)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chaos-drop)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chaos-corrupt)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chaos-close)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chaos-close-mode)
                    COMPREPLY=($(compgen -W "abrupt graceful" -- "${cur}"))
                    return 0
                    ;;
                --chaos-read-pause)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chaos-seed)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --output)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -o)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --prometheus-listen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --otel-endpoint)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --otel-interval)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --otel-service-name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --scenario)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --phase)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --agents)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --agent-token)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --agent-timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --agent-ca)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        wsblast__subcmd__agent)
            opts="-h --listen --token --tls-cert --tls-key --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --listen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --token)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tls-cert)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tls-key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        wsblast__subcmd__mock)
            opts="-h --listen --delay --jitter --close-after --reject-ratio --stall-every --stall-for --replay --rules --socketio --tls-cert --tls-key --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --listen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --delay)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --jitter)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --close-after)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --reject-ratio)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --stall-every)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --stall-for)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --replay)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --rules)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tls-cert)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tls-key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        wsblast__subcmd__render)
            opts="-o -h --format --output --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --format)
                    COMPREPLY=($(compgen -W "html markdown text" -- "${cur}"))
                    return 0
                    ;;
                --output)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -o)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _wsblast -o nosort -o bashdefault -o default wsblast
else
    complete -F _wsblast -o bashdefault -o default wsblast
fi
