#!/bin/sh
# Fewer: the ready-made programs for this computer, then Fewer put into Claude Code.
#
#   curl -fsSL https://github.com/hazemdawahi/fewer/releases/latest/download/install.sh | sh
#   ... | sh -s -- --yes          answer install's one question for you (Claude Code, scripts)
#
# It downloads Fewer's two programs (fewer and fewer-pack) for this system from the latest
# release, checks them against the release's SHA256SUMS, puts them in ~/.local/bin (or
# FEWER_BIN_DIR), adds that folder to your PATH when it is not there yet, then runs
# `fewer install`, which shows every change it will make and asks once. Nothing else is needed:
# no Rust, no git. Later: `fewer update` does this again; `fewer uninstall` takes it all out.
# (Published as the release's install.sh; building from the source instead: install.sh.)
set -eu

repo=hazemdawahi/fewer
base=${FEWER_DOWNLOAD:-https://github.com/$repo/releases/latest/download}

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Linux-x86_64 | Linux-amd64) target=x86_64-unknown-linux-musl ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
    *) die "there is no ready-made Fewer for $os on $arch yet. Build it from the source: https://github.com/$repo" ;;
esac
command -v curl >/dev/null 2>&1 || die "curl is needed to download Fewer"

bin=${FEWER_BIN_DIR:-$HOME/.local/bin}
tmp=$(mktemp -d)
tarball=fewer-$target.tar.gz
say "Downloading Fewer for $os ($arch)..."
curl -fsSL "$base/$tarball" -o "$tmp/$tarball" || die "could not download $base/$tarball"
# (the release's checksums: the download is the one published)
if curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" 2>/dev/null; then
    want=$(grep " $tarball\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
    if command -v shasum >/dev/null 2>&1; then
        got=$(shasum -a 256 "$tmp/$tarball" | cut -d' ' -f1)
    else
        got=$(sha256sum "$tmp/$tarball" | cut -d' ' -f1)
    fi
    [ -n "$want" ] && [ "$want" = "$got" ] || die "the download does not match the release's checksum; nothing was installed"
fi
tar -xzf "$tmp/$tarball" -C "$tmp" fewer fewer-pack
mkdir -p "$bin"
# (a copy beside the old one, then a rename: a program in use is never half written)
for name in fewer fewer-pack; do
    cp "$tmp/$name" "$bin/.$name.new"
    chmod 755 "$bin/.$name.new"
    mv -f "$bin/.$name.new" "$bin/$name"
done
rm -f "$tmp/$tarball" "$tmp/SHA256SUMS" "$tmp/fewer" "$tmp/fewer-pack"
rmdir "$tmp"
say "Installed $("$bin/fewer" --version) and fewer-pack in $bin."

# (the folder on PATH: added to the shell's startup file when it is not there yet, with a mark
# that `fewer uninstall` finds; new terminals see it)
rc=""
case ":$PATH:" in
    *":$bin:"*) ;;
    *)
        case "${SHELL:-}" in
            */zsh) rc=$HOME/.zshrc ;;
            */bash) if [ "$os" = Darwin ]; then rc=$HOME/.bash_profile; else rc=$HOME/.bashrc; fi ;;
            *) rc=$HOME/.profile ;;
        esac
        if ! grep -qs "^# fewer: its programs on PATH" "$rc"; then
            printf '\n# fewer: its programs on PATH (fewer uninstall takes this out)\nexport PATH="%s:$PATH"\n' "$bin" >> "$rc"
            say "Added $bin to your PATH in $rc (new terminals see it)."
        fi
        ;;
esac
say ""

# Claude Code's side: shown first, one question (or --yes). Piped into sh, this script's input
# is the download, so the question is asked on the terminal itself when there is one.
export FEWER_FROM=release FEWER_BIN_DIR="$bin" FEWER_PATH_FILE="$rc"
if [ -t 1 ] && { : </dev/tty; } 2>/dev/null; then
    "$bin/fewer" install "$@" </dev/tty
else
    "$bin/fewer" install "$@"
fi
