# 대화형 Bash에서 명시적으로 source 한다. Ctrl-X p: 페어링, Ctrl-X u: 승인 작업 호출.
[[ $- == *i* && ${BASH_VERSION-} && $BASH_SUBSHELL == 0 ]] || return 0

__zzabba_shared_pair() {
    local code= char locator end=$((SECONDS + 60))
    # bind -x 처리기 안에서는 readline 터미널 설정이 유지된다. Enter는 클라이언트에 따라 CR 또는 LF로 온다.
    printf 'Zzabba pairing code: ' >/dev/tty
    while (( SECONDS < end )) && IFS= read -r -s -n 1 -t "$((end - SECONDS))" char </dev/tty; do
        [[ -z $char || $char == $'\r' ]] && break
        code+=$char
        (( ${#code} <= 96 )) || return 1
    done
    printf '\n' >/dev/tty
    code=${code#$'\e[200~'}
    code=${code%$'\e[201~'}
    [[ $code =~ ^[0-9a-f]{64}$ ]] || return 1
    locator=$(exec -c /usr/local/libexec/zzabba-ai-control --shared-pair <<<"$BASHPID_FOR_ZZABBA"$'\n'"$code") || return
    __zzabba_shared_locator=$locator
}

__zzabba_shared_key() {
    [[ $BASH_SUBSHELL == 0 && ${READLINE_LINE+x} && -z $READLINE_LINE ]] || return 0
    local restore_trace=0 restore_monitor=0 BASHPID_FOR_ZZABBA=$BASHPID
    if [[ $- == *x* ]]; then restore_trace=1; set +x; fi
    # 동기 도우미가 Bash와 동일한 포그라운드 프로세스 그룹을 유지한다.
    if [[ $- == *m* ]]; then restore_monitor=1; set +m; fi
    case $1 in
        pair) __zzabba_shared_pair || : ;;
        claim)
            if [[ ${__zzabba_shared_locator-} =~ ^[0-9a-f]{32}$ ]]; then
                (exec -c /usr/local/libexec/zzabba-ai-control --shared-claim <<<"$BASHPID_FOR_ZZABBA"$'\n'"$__zzabba_shared_locator") || :
            fi
            ;;
    esac
    if (( restore_monitor )); then set -m; fi
    if (( restore_trace )); then set -x; fi
    return 0
}

bind -x '"\C-xp":__zzabba_shared_key pair'
bind -x '"\C-xu":__zzabba_shared_key claim'
