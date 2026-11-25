#!/bin/sh
# Build a binary .deb of seance for this machine's architecture.
#   packaging/deb.sh            -> target/deb/seance_<version>-1_<arch>.deb
#
# Works on any Linux: it uses dpkg-deb when present and otherwise assembles the package
# with ar and tar (a .deb is an ar archive of debian-binary, control.tar.xz, data.tar.xz).
# The libc6 dependency is the newest GLIBC symbol version the binary uses, so a package
# built on a new distribution says so instead of failing at run time on an older one.
# Build on the oldest release you want to support (a Debian/Ubuntu container works) for
# the widest reach.
set -eu

cd "$(dirname "$0")/.."
root=$PWD
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
case "$(uname -m)" in
    x86_64) arch=amd64 ;;
    aarch64 | arm64) arch=arm64 ;;
    armv7l) arch=armhf ;;
    riscv64) arch=riscv64 ;;
    ppc64le) arch=ppc64el ;;
    *) echo "unknown architecture $(uname -m)" >&2; exit 1 ;;
esac

CARGO_TARGET_DIR=target/deb-build cargo build --release --locked
bin=target/deb-build/release/seance

out="$root/target/deb"
stage="$out/stage"
rm -rf "$stage"
mkdir -p "$stage/DEBIAN" "$stage/usr/bin" "$stage/usr/share/doc/seance"
install -m 0755 "$bin" "$stage/usr/bin/seance"
install -m 0644 README.md "$stage/usr/share/doc/seance/README.md"
{
    echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
    echo "Upstream-Name: seance"
    echo "Source: https://github.com/kernelzeroday/seance"
    echo
    echo "Files: *"
    echo "Copyright: seance contributors"
    echo "License: Apache-2.0"
    echo " On Debian systems, the full text of the Apache License, Version 2.0 can be"
    echo " found in /usr/share/common-licenses/Apache-2.0."
} >"$stage/usr/share/doc/seance/copyright"

glibc=$(objdump -T "$bin" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n1)
depends="libc6${glibc:+ (>= $glibc)}"
size=$(du -sk "$stage/usr" | cut -f1)

cat >"$stage/DEBIAN/control" <<EOF
Package: seance
Version: $version-1
Architecture: $arch
Maintainer: seance contributors <noreply@github.com>
Homepage: https://github.com/kernelzeroday/seance
Installed-Size: $size
Depends: $depends
Section: utils
Priority: optional
Description: one picker for every coding agent's sessions
 seance lists the sessions that coding agents leave behind (Claude Code, Codex,
 Antigravity, oh-my-pi, pi, Hermes, fx, goose, OpenCode, sparkle and sd) in one
 filterable terminal picker with a live preview of each conversation. Enter
 resumes a session in its own folder; the new-session menu starts any installed
 agent here or in a folder picked from a built-in browser.
EOF
(cd "$stage" && find usr -type f -exec md5sum {} + | sort -k2) >"$stage/DEBIAN/md5sums"

deb="$out/seance_${version}-1_${arch}.deb"
rm -f "$deb"
if command -v dpkg-deb >/dev/null; then
    dpkg-deb --root-owner-group -Zxz --build "$stage" "$deb" >/dev/null
else
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    printf '2.0\n' >"$work/debian-binary"
    tar -C "$stage/DEBIAN" --owner=0 --group=0 --numeric-owner --sort=name \
        -cJf "$work/control.tar.xz" ./control ./md5sums
    tar -C "$stage" --owner=0 --group=0 --numeric-owner --sort=name --exclude=./DEBIAN \
        -cJf "$work/data.tar.xz" ./usr
    (cd "$work" && ar rc "$deb" debian-binary control.tar.xz data.tar.xz)
fi
rm -rf "$stage"
echo "built $deb  (Depends: $depends)"
