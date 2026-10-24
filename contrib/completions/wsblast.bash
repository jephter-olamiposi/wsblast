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
            *)
                ;;
        esac
    done

    case "${cmd}" in
        wsblast)
            opts="-u -c -d -n -r -p -H -o -h -V --url --connections --ramp-rate --duration --requests --rate --payload --payload-file --binary --header --subprotocol --mode --connect-timeout --message-timeout --match --ping-interval --max-p50 --max-p95 --max-p99 --max-p999 --max-error-rate --min-throughput --auth-type --auth-token --auth-header --auth-apikey --auth-apikey-header --graphql-ws-operation-name --graphql-ws-operation --graphql-ws-variables --graphql-ws-extensions --format --compare --compare-threshold --fail-on-regression --output --tui --no-progress --no-reconnect --scenario --phase --help --version"
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
                --mode)
                    COMPREPLY=($(compgen -W "echo stream listen graphql-ws" -- "${cur}"))
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
                --auth-type)
                    COMPREPLY=($(compgen -W "none static apikey" -- "${cur}"))
                    return 0
                    ;;
                --auth-token)
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
                --format)
                    COMPREPLY=($(compgen -W "text json markdown" -- "${cur}"))
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
                --output)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -o)
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
