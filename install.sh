#!/bin/sh
set -eu

usage() {
    cat <<'EOF'
Usage: ./install.sh [OPTIONS]

Builds kmx with cargo and installs it, with its bash, zsh and fish
completions, so that `kmx` runs from any directory.

Options:
  --user          install under ~/.local instead of /usr/local (no root needed)
  --prefix DIR    install under DIR (default: /usr/local, or ~/.local with --user)
  --destdir DIR   stage the files under DIR for packaging; paths keep the prefix
  --no-build      install the existing target/release/kmx without running cargo
  --uninstall     remove the files that this script installs
  -h, --help      show this help

Building needs Rust 1.91 or newer (https://rustup.rs). The build always runs
as the invoking user; only copying into a directory that the user cannot write
asks for sudo or doas.
EOF
}

die() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

say() {
    printf '%s\n' "$*"
}

prefix=""
user=0
destdir=""
build=1
action=install
while [ $# -gt 0 ]; do
    case $1 in
        --user) user=1 ;;
        --prefix)
            [ $# -ge 2 ] || die "--prefix needs a directory"
            prefix=$2
            shift
            ;;
        --prefix=*) prefix=${1#--prefix=} ;;
        --destdir)
            [ $# -ge 2 ] || die "--destdir needs a directory"
            destdir=$2
            shift
            ;;
        --destdir=*) destdir=${1#--destdir=} ;;
        --no-build) build=0 ;;
        --uninstall) action=uninstall ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown option $1 (see ./install.sh --help)" ;;
    esac
    shift
done

if [ -z "$prefix" ]; then
    if [ "$user" -eq 1 ]; then
        [ -n "${HOME:-}" ] || die "HOME is not set, so --user has no home to install into"
        prefix=$HOME/.local
    else
        prefix=/usr/local
    fi
fi
case $prefix in
    /*) ;;
    *) die "--prefix must be an absolute path, not $prefix" ;;
esac
case $destdir in
    "" | /*) ;;
    *) die "--destdir must be an absolute path, not $destdir" ;;
esac

here=$(cd "$(dirname "$0")" && pwd)
[ -f "$here/Cargo.toml" ] && [ -d "$here/src/cmd/completion" ] ||
    die "run install.sh from a kmx source checkout ($here is not one)"

data=${KMX_HOME:-${XDG_DATA_HOME:-${HOME:-~}/.local/share}/kmx}
bindir=$prefix/bin
bash_dir=$prefix/share/bash-completion/completions
zsh_dir=$prefix/share/zsh/site-functions
fish_dir=$prefix/share/fish/vendor_completions.d
doc_dir=$prefix/share/doc/kmx

elevate=""
choose_elevation() {
    probe=$destdir$prefix
    while [ ! -e "$probe" ]; do
        probe=$(dirname "$probe")
    done
    if [ -w "$probe" ]; then
        return
    fi
    if command -v sudo >/dev/null 2>&1; then
        elevate=sudo
    elif command -v doas >/dev/null 2>&1; then
        elevate=doas
    else
        die "$probe is not writable and neither sudo nor doas is available; rerun as root or use --user"
    fi
    say "installing into $destdir$prefix needs root; using $elevate"
}

run() {
    if [ -n "$elevate" ]; then
        "$elevate" "$@"
    else
        "$@"
    fi
}

if [ "$action" = uninstall ]; then
    choose_elevation
    removed=0
    for f in "$bindir/kmx" "$bash_dir/kmx" "$zsh_dir/_kmx" "$fish_dir/kmx.fish" \
        "$doc_dir/README.md" "$doc_dir/LICENSE"; do
        if [ -e "$destdir$f" ] || [ -L "$destdir$f" ]; then
            run rm -f "$destdir$f"
            say "removed $destdir$f"
            removed=$((removed + 1))
        fi
    done
    if [ -d "$destdir$doc_dir" ]; then
        run rmdir "$destdir$doc_dir" 2>/dev/null || true
    fi
    [ "$removed" -gt 0 ] || say "nothing to remove under $destdir$prefix"
    say "indexes stay in $data; delete them first with kmx rm ID... or remove that directory"
    exit 0
fi

if [ "$build" -eq 1 ]; then
    if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ]; then
        die "do not run install.sh with sudo: the build would leave root-owned files in $here. Run it as $SUDO_USER; it asks for root only to copy files (or build first and pass --no-build)"
    fi
    command -v cargo >/dev/null 2>&1 ||
        die "cargo is not installed; install Rust 1.91 or newer (https://rustup.rs) and rerun"
    version=$(cargo --version | awk '{print $2}')
    major=${version%%.*}
    rest=${version#*.}
    minor=${rest%%.*}
    case $major$minor in
        *[!0-9]* | "") die "cannot read the cargo version from \"$(cargo --version)\"" ;;
    esac
    if [ "$major" -lt 1 ] || { [ "$major" -eq 1 ] && [ "$minor" -lt 91 ]; }; then
        die "kmx needs Rust 1.91 or newer, but cargo is $version; update with rustup update"
    fi
    say "building kmx with cargo $version"
    (cd "$here" && cargo build --release --locked)
fi

bin=$here/target/release/kmx
[ -x "$bin" ] || die "$bin does not exist; run without --no-build"
version=$("$bin" --version 2>/dev/null) || die "$bin does not run on this machine; rebuild it here"

choose_elevation
run install -d "$destdir$bindir" "$destdir$bash_dir" "$destdir$zsh_dir" \
    "$destdir$fish_dir" "$destdir$doc_dir"
staged=$destdir$bindir/.kmx.new.$$
run install -m 755 "$bin" "$staged"
run mv -f "$staged" "$destdir$bindir/kmx"
run install -m 644 "$here/src/cmd/completion/kmx.bash" "$destdir$bash_dir/kmx"
run install -m 644 "$here/src/cmd/completion/kmx.zsh" "$destdir$zsh_dir/_kmx"
run install -m 644 "$here/src/cmd/completion/kmx.fish" "$destdir$fish_dir/kmx.fish"
run install -m 644 "$here/README.md" "$destdir$doc_dir/README.md"
run install -m 644 "$here/LICENSE" "$destdir$doc_dir/LICENSE"

say "installed $version"
say "  existing indexes and caches in $data are kept and reused"
say "  $destdir$bindir/kmx"
say "  completions: $destdir$bash_dir/kmx, $destdir$zsh_dir/_kmx, $destdir$fish_dir/kmx.fish"
[ -z "$destdir" ] || exit 0

case ":$PATH:" in
    *":$bindir:"*) ;;
    *) say "note: $bindir is not on PATH; add it, for example: export PATH=\"$bindir:\$PATH\"" ;;
esac
found=$(command -v kmx 2>/dev/null || true)
if [ -n "$found" ] && [ "$found" != "$bindir/kmx" ]; then
    say "note: $found comes first on PATH and hides $bindir/kmx"
fi
if command -v zsh >/dev/null 2>&1; then
    if ! zsh -fc 'print -l -- $fpath' 2>/dev/null | grep -qx "$zsh_dir"; then
        say "note: zsh does not search $zsh_dir; add fpath+=($zsh_dir) before compinit in ~/.zshrc"
    fi
fi
say "open a new shell to pick up Tab completion"
