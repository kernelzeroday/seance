# Built in place from the source tree by packaging/rpm.sh (rpmbuild --build-in-place):
# no tarball, cargo fetches dependencies as for a normal build.

%global debug_package %{nil}

Name:           seance
Version:        %{?pkgver}%{!?pkgver:0.0.0}
Release:        1%{?dist}
Summary:        One picker for every coding agent's sessions: preview, resume, or start new
License:        Apache-2.0
URL:            https://github.com/kernelzeroday/seance

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
Recommends:     wl-clipboard

%description
seance lists the sessions that coding agents leave behind (Claude Code, Codex,
Antigravity, oh-my-pi, pi, Hermes, fx, goose, OpenCode, sparkle and sd) in one
filterable terminal picker with a live preview of each conversation. Enter resumes
a session in its own folder; the new-session menu starts any installed agent here
or in a folder picked from a built-in browser. It only reads the agents' files.

%build
# A separate target dir keeps rpm's build flags from invalidating the normal cargo build.
CARGO_TARGET_DIR=target/rpm cargo build --release --locked

%check
CARGO_TARGET_DIR=target/rpm cargo test --release --locked

%install
install -Dpm 0755 target/rpm/release/seance %{buildroot}%{_bindir}/seance

%files
%license LICENSE
%doc README.md
%{_bindir}/seance

%changelog
* Tue Nov 24 2026 kernelzeroday - 0.1.1-1
- ^Y copies the previewed conversation with wl-copy (xclip/xsel, OSC 52 fallback);
  ^E copies the resume command; seance show --copy.

* Tue Nov 24 2026 kernelzeroday - 0.1.0-1
- First release: sessions of claude, codex, agy, omp, pi, hermes, fx, goose, opencode,
  sparkle and sd; preview, resume, new session with folder browser.
