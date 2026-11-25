#!/bin/sh
# Build the seance RPM from this source tree; with --install, install it with pkexec + dnf.
#   packaging/rpm.sh            -> target/rpmbuild/RPMS/<arch>/seance-<version>-1.<dist>.<arch>.rpm
#   packaging/rpm.sh --install
set -eu

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
top="$PWD/target/rpmbuild"

rpmbuild -bb --build-in-place \
    --define "_topdir $top" \
    --define "pkgver $version" \
    packaging/seance.spec

rpm=$(ls -t "$top"/RPMS/*/seance-"$version"-*.rpm | head -n1)
echo "built $rpm"

if [ "${1:-}" = "--install" ]; then
    # The same version again (a rebuild) needs reinstall; dnf install would do nothing.
    if rpm -q --quiet "$(rpm -qp --qf '%{NEVRA}' "$rpm")"; then
        pkexec dnf reinstall -y "$rpm"
    else
        pkexec dnf install -y "$rpm"
    fi
fi
