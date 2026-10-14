_kmx() {
    local cur=${COMP_WORDS[COMP_CWORD]}
    local cmd=${COMP_WORDS[0]}
    [[ $cmd == "~"* ]] && cmd=$HOME${cmd:1}
    local -a out
    mapfile -t out < <("$cmd" --complete-line "${COMP_LINE:0:COMP_POINT}" 2>/dev/null)
    local token=${out[0]}
    out=("${out[@]:1}")
    COMPREPLY=()
    if [[ ${out[0]} == :dirs ]]; then
        compopt -o filenames 2>/dev/null
        mapfile -t COMPREPLY < <(compgen -d -- "$cur")
        COMPREPLY+=("${out[@]:1}")
        return
    fi
    local pre=${token%"$cur"}
    COMPREPLY=("${out[@]#"$pre"}")
    if [[ ${#COMPREPLY[@]} -eq 1 && ${COMPREPLY[0]} == *[/=] ]]; then
        compopt -o nospace 2>/dev/null
    fi
}

complete -F _kmx kmx
