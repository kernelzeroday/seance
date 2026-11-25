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
seance lists the sessions that coding agents leave behind (Claude Code, Claurst, Codex,
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
* Wed Nov 25 2026 kernelzeroday - 0.1.7-1
- agy: an empty write-ahead log no longer makes old conversations look recent.

* Wed Nov 25 2026 kernelzeroday - 0.1.6-1
- Come back to the picker when an agent exits (--once / return = false for the old
  hand-off); Ctrl+C reaches only the agent while it runs.

* Wed Nov 25 2026 kernelzeroday - 0.1.5-1
- Live updates keep the cursor on the selected session instead of drifting; active
  claurst, agy and fx sessions now move to the top while they are working.

* Tue Nov 24 2026 kernelzeroday - 0.1.4-1
- Claurst sessions: snapshot plus journal replay, live updates, claurst --resume.

* Tue Nov 24 2026 kernelzeroday - 0.1.3-1
- Live updates with inotify: new and changed sessions appear without polling, and the
  open preview streams new messages; redraws only when something changes.

* Tue Nov 24 2026 kernelzeroday - 0.1.2-1
- Ctrl+C / Ctrl+D must be pressed twice in quick succession to quit; the first
  Ctrl+C clears the filter.
- Mouse capture by default: click and double-click tabs, sessions and menu rows, wheel
  scrolling, and drag-to-copy of preview text only (double-click copies a message).

* Tue Nov 24 2026 kernelzeroday - 0.1.1-1
- ^Y copies the previewed conversation with wl-copy (xclip/xsel, OSC 52 fallback);
  ^E copies the resume command; seance show --copy.

* Tue Nov 24 2026 kernelzeroday - 0.1.0-1
- First release: sessions of claude, codex, agy, omp, pi, hermes, fx, goose, opencode,
  sparkle and sd; preview, resume, new session with folder browser.
