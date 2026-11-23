# 대화형 Bash에서 명시적으로 source 한다. Ctrl-X p: 페어링, Ctrl-X u: 승인 작업 호출.
[[ $- == *i* && ${BASH_VERSION-} && $BASH_SUBSHELL == 0 ]] || return 0

__zzabba_shared_pair() {
    local code= char locator now end paste=0 reject=0 entered=0
    printf -v end '%(%s)T' -1
    end=$((end + 60))
    # Enter는 수신 시점의 터미널 설정에 따라 CR 또는 LF(빈 문자)로 온다.
    # 괄호 붙여넣기는 끝 표지까지 읽는다. 안쪽 줄바꿈이나 길이 초과는 거부하되 끝 표지까지 계속 읽어 버린다.
    printf 'Zzabba pairing code: ' >/dev/tty
    while printf -v now '%(%s)T' -1 && (( now < end )) &&
        IFS= read -r -s -n 1 -t "$((end - now))" char </dev/tty; do
        [[ -z $char ]] && char=$'\n'
        if (( paste )); then
            code+=$char
            if [[ $code == *$'\e[201~' ]]; then entered=1; break; fi
            if (( ${#code} > 96 )); then reject=1; code=${code: -5}; fi
            continue
        fi
        if [[ $char == $'\n' || $char == $'\r' ]]; then entered=1; break; fi
        code+=$char
        [[ $code == $'\e[200~' ]] && paste=1
        (( ${#code} <= 96 )) || { reject=1; break; }
    done
    # 남은 입력이 readline에 넘어가 명령으로 실행되지 않도록 버린다.
    while IFS= read -r -s -n 1 -t 0.05 char </dev/tty; do :; done
    printf '\n' >/dev/tty
    (( entered && !reject )) || return 1
    if (( paste )); then
        code=${code#$'\e[200~'}
        code=${code%$'\e[201~'}
        # 줄 단위 복사로 붙은 끝 줄바꿈 한 개만 허용한다.
        [[ $code == *$'\r' || $code == *$'\n' ]] && code=${code:0:${#code}-1}
    fi
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
