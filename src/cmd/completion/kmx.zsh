#compdef kmx

_kmx() {
    local -a out bare spaced
    local cmd=${(Q)words[1]}
    [[ $cmd == '~'* ]] && cmd=$HOME${cmd#'~'}
    out=("${(@f)$("$cmd" --complete $((CURRENT - 1)) -- "${words[@]}" 2>/dev/null)}")
    out=(${out:#})
    if [[ ${out[1]} == :dirs ]]; then
        _files -/
        (( ${#out} > 1 )) && compadd -Q -- "${out[@]:1}"
        return 0
    fi
    bare=(${(M)out:#*[/=]})
    spaced=(${out:#*[/=]})
    (( ${#spaced} )) && compadd -Q -- "${spaced[@]}"
    (( ${#bare} )) && compadd -Q -S '' -- "${bare[@]}"
    return 0
}

_kmx "$@"
