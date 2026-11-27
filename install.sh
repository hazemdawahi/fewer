#!/bin/sh
# fewer: build it, put it on your PATH, and put it into Claude Code.
#
#   sh install.sh [--yes] [PREFIX]    build fewer and fewer-pack, put them in PREFIX/bin (default
#                                     ~/.local; PREFIX=... sh install.sh works too), add that
#                                     folder to your PATH when it is not there, then run
#                                     `fewer install` (it shows every change and asks once;
#                                     --yes answers for you, for Claude Code or a script)
#
# Later: `fewer update` gets the new version and does all of this again; `fewer uninstall` takes
# fewer out of Claude Code and removes it (it asks before deleting what fewer saved).
# For a system-wide install, build as yourself first, then copy as root:
#   sh install.sh && sudo sh install.sh /usr/local
# (run as root, or where cargo is not found, it installs the existing builds instead of building).
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

yes=""
if [ "${1:-}" = "--yes" ]; then
    yes="--yes"
    shift
fi
[ $# -le 1 ] || die "one PREFIX only (got: $*) (sh install.sh [--yes] [PREFIX])"
case "${1:-}" in
    -*) die "unknown option: $1 (sh install.sh [--yes] [PREFIX])" ;;
esac
p=${1:-${PREFIX:-$HOME/.local}}
case "$p" in
    /*) ;;
    *) p=$(pwd)/$p ;;
esac
bin=${p%/}/bin

# (the builds always go to the project's own target folders, whatever CARGO_TARGET_DIR says)
main_bin=$here/target/release/fewer-pack
search_bin=$here/search/target/release/fewer
if [ "$(id -u)" = 0 ] && [ -x "$main_bin" ] && [ -x "$search_bin" ]; then
    say "Using the builds in $here (run 'sh install.sh' as yourself first to rebuild them)."
elif command -v cargo >/dev/null 2>&1; then
    need=89
    have=$(rustc --version 2>/dev/null | sed -n 's/^rustc 1\.\([0-9][0-9]*\).*/\1/p')
    [ -n "$have" ] && [ "$have" -ge "$need" ] || die "Rust 1.$need or newer is needed ($(rustc --version 2>/dev/null || echo 'rustc not found')): rustup update"
    say "Building fewer-pack (the packer), about a minute..."
    (cd "$here" && cargo build --release --quiet --target-dir "$here/target")
    say "Building fewer, about a minute..."
    (cd "$here/search" && cargo build --release --quiet --target-dir "$here/search/target")
elif [ -x "$main_bin" ] && [ -x "$search_bin" ]; then
    say "cargo not found; using the existing builds in $here."
else
    die "Rust is needed to build fewer (cargo was not found): https://rustup.rs"
fi

mkdir -p "$bin"
# (a copy beside the old one, then a rename: a program in use is never half written)
for src in "$main_bin" "$search_bin"; do
    name=${src##*/}
    cp "$src" "$bin/.$name.new"
    chmod 755 "$bin/.$name.new"
    mv -f "$bin/.$name.new" "$bin/$name"
done
say "Installed $("$bin/fewer" --version) and fewer-pack in $bin."

# (the folder on PATH: added to the shell's startup file when it is not there yet, with a mark
# that `fewer uninstall` finds; new terminals see it)
rc=""
case ":$PATH:" in
    *":$bin:"*) ;;
    *)
        case "${SHELL:-}" in
            */zsh) rc=$HOME/.zshrc ;;
            */bash) if [ "$(uname)" = Darwin ]; then rc=$HOME/.bash_profile; else rc=$HOME/.bashrc; fi ;;
            *) rc=$HOME/.profile ;;
        esac
        if ! grep -qs "^# fewer: its programs on PATH" "$rc"; then
            printf '\n# fewer: its programs on PATH (fewer uninstall takes this out)\nexport PATH="%s:$PATH"\n' "$bin" >> "$rc"
            say "Added $bin to your PATH in $rc (new terminals see it)."
        fi
        ;;
esac
say ""

# Claude Code's side: shown first, one question (or --yes); the folder fewer was built from and
# the one it is in, recorded for `fewer update` and `fewer uninstall`
FEWER_FROM=$here FEWER_BIN_DIR=$bin FEWER_PATH_FILE=$rc "$bin/fewer" install $yes
