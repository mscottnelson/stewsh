# StewardShell: source after installing stewsh. No shell configuration is edited.
# Command text is opt-in: export STEWSH_RECORD_COMMANDS=1 before sourcing.
[[ -o interactive ]] || return
(( $+commands[stewsh] )) || return
if [[ -n ${ITERM_SESSION_ID-} ]]; then
    export STEWSH_SESSION_ID="iterm:${ITERM_SESSION_ID##*:}"
else
    export STEWSH_SESSION_ID="zsh:${TTY:-unknown}:$$"
fi
autoload -Uz add-zsh-hook
_stewsh_preexec() {
    unset _stewsh_pending _stewsh_command
    # Overseer actions should not turn their own terminal into a new loose end.
    case "$1" in
        stewsh|stewsh\ *|command\ stewsh\ *) return 0 ;;
    esac
    typeset -g _stewsh_pending=1
    local -a track_args
    track_args=(--passive track "$STEWSH_SESSION_ID" --state working)
    if [[ ${STEWSH_RECORD_COMMANDS:-0} == 1 ]]; then
        typeset -g _stewsh_command="$1"
        track_args+=(--command "$1")
    fi
    command stewsh "${track_args[@]}" >/dev/null 2>&1
    return 0
}
_stewsh_precmd() {
    local status_code=$?
    if [[ -n ${_stewsh_pending-} ]]; then
        local -a track_args
        track_args=(--passive track "$STEWSH_SESSION_ID" --exit-code "$status_code")
        [[ -n ${_stewsh_command-} ]] && track_args+=(--command "$_stewsh_command")
        command stewsh "${track_args[@]}" >/dev/null 2>&1
        unset _stewsh_pending _stewsh_command
    fi
    return "$status_code"
}
_stewsh_chpwd() {
    command stewsh --passive track "$STEWSH_SESSION_ID" --state idle >/dev/null 2>&1
    return 0
}
_stewsh_exit() {
    # A nested shell exiting does not close an iTerm pane. Let sync verify it.
    [[ -n ${ITERM_SESSION_ID-} ]] && return 0
    command stewsh --passive close "$STEWSH_SESSION_ID" >/dev/null 2>&1
    return 0
}
# add-zsh-hook does not duplicate an already registered function.
add-zsh-hook preexec _stewsh_preexec
add-zsh-hook precmd _stewsh_precmd
add-zsh-hook chpwd _stewsh_chpwd
add-zsh-hook zshexit _stewsh_exit
command stewsh --passive track "$STEWSH_SESSION_ID" >/dev/null 2>&1
