function __kmx_complete
    set -l tokens (commandline -opc)
    set -l current (commandline -ct)
    set -l cmd (string replace -r '^~' $HOME -- $tokens[1])
    set -l out ($cmd --complete (count $tokens) -- $tokens $current 2>/dev/null)
    if test "$out[1]" = ":dirs"
        __fish_complete_directories $current
        printf '%s\n' $out[2..-1]
    else
        printf '%s\n' $out
    end
end

complete -c kmx -f -a '(__kmx_complete)'
