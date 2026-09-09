# Source from ~/.zshrc after installing stewsh. Does not alter your config itself.
[[ -o interactive ]] || return
(( $+commands[stewsh] )) || return
export STEWSH_SESSION_ID="${TTY:-zsh}:$$"
autoload -Uz add-zsh-hook
_stewsh_preexec() {
    typeset -g _stewsh_command="$1"
    command stewsh track "$STEWSH_SESSION_ID" --command "$1" >/dev/null 2>&1
}
_stewsh_precmd() {
    local status_code=$?
    if [[ -n ${_stewsh_command-} ]]; then
        command stewsh track "$STEWSH_SESSION_ID" --command "$_stewsh_command" --exit-code "$status_code" >/dev/null 2>&1
        unset _stewsh_command
    fi
    return "$status_code"
}
_stewsh_chpwd() {
    command stewsh track "$STEWSH_SESSION_ID" >/dev/null 2>&1
}
add-zsh-hook preexec _stewsh_preexec
add-zsh-hook precmd _stewsh_precmd
add-zsh-hook chpwd _stewsh_chpwd
_stewsh_chpwd
