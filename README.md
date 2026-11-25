# seance

One picker for the sessions every coding agent leaves behind. Browse them all in one
list, preview the conversation, press Enter to pick up where you left off, or start a
new session with any agent, here or in a folder you browse to.

```
 seance   142 sessions                                                          ~/src/app
  all 142   claude 51   codex 18   agy 12   omp 9   pi 22   hermes 4   fx 7   goose 3 …
╭──────────────────────────────────────────╮╭────────────────────────────────────────────╮
│ ⌕ flaky▏  2 matches                      ││ Fix flaky login test                       │
│                                          ││ ● codex  ·  2d ago  ·  ~/src/app  ·  gpt-5 │
│ ＋ New session                           ││ ↵ codex resume 019a3c1e-…  in ~/src/app    │
│    any agent, here or in a folder you pi ││ ────────────────────────────────────────── │
│▌● Fix flaky login test               2d  ││ › you                                      │
│▌   codex  ~/src/app · 14 msg · gpt-5     ││ │ the login e2e test fails one run in five │
╰──────────────────────────────────────────╯╰────────────────────────────────────────────╯
 ↵ resume   ^N new   ^F new in folder   ⇥ agent   ^O here only   PgUp/Dn scroll   ? help
```

## Agents

| agent      | sessions read from                                   | resumed with                              |
|------------|------------------------------------------------------|-------------------------------------------|
| `claude`   | `~/.claude/projects/*/<id>.jsonl`                    | `claude --resume <id>` (in its folder)    |
| `codex`    | `~/.codex/sessions/**/rollout-*.jsonl`               | `codex resume <id>`                       |
| `agy`      | `~/.gemini/antigravity-cli/` (metadata, history, db) | `agy --conversation <id>`                 |
| `omp`      | `~/.omp/agent/sessions/*/*.jsonl`                    | `omp --resume <file>`                     |
| `pi`       | `~/.pi/agent/sessions/*/*.jsonl`                     | `pi --session <file>`                     |
| `hermes`   | `~/.hermes/state.db`                                 | `hermes --resume <id>`                    |
| `fx`       | `~/.fx/sessions/v2/`                                 | `fx session resume <id>`                  |
| `goose`    | `~/.local/share/goose/sessions/sessions.db`          | `goose session --resume --session-id <id>`|
| `opencode` | `~/.local/share/opencode/opencode.db`                | `opencode --session <id>`                 |
| `sparkle`  | `~/.local/share/sparkle/sparkle.db`                  | `sparkle chat --cid <id>`                 |
| `sd`       | `~/.local/share/sd-tui/sessions/<id>.jsonl`          | `sd --resume <id>`                        |

`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `HERMES_HOME`, `PI_CODING_AGENT_SESSION_DIR` and
`XDG_DATA_HOME` are honoured. seance only ever reads these files and databases (SQLite is
opened read-only). Parsed file sessions are cached in `~/.cache/seance/index.json` by size
and modification time, so a rescan only reads what changed.

Antigravity keeps its steps as protobuf without a published schema; its preview shows the
prompts, the assistant's prose and the tool calls it can read from them.

## Keys

| key            | does                                                           |
|----------------|----------------------------------------------------------------|
| type           | filter (every word must match title, prompt, folder, model or id) |
| ↑ ↓, ^K ^J     | move                                                           |
| Enter          | resume the highlighted session; on **＋ New session**, start one |
| Tab, ← →       | switch agent tab                                               |
| ^N             | new session: pick an agent; Enter starts it in the current folder, Tab opens the folder browser |
| ^F             | new session with the highlighted session's agent, browsing from its folder |
| ^O             | only sessions under the current folder                         |
| PgUp PgDn, Shift+↑↓ | scroll the preview                                        |
| ^V             | show or hide the preview                                       |
| ^Y             | copy the conversation to the clipboard (`wl-copy` on Wayland, `xclip`/`xsel` on X11, else OSC 52) |
| ^E             | copy the resume command                                        |
| ^R             | rescan                                                         |
| Esc            | clear the filter, then quit                                    |
| ^C ^C, ^D ^D   | quit (press twice quickly; the first ^C also clears the filter) |

### Mouse

The mouse is captured by default, so it works like the keyboard: click a tab or a session
to select it, double-click to resume (or to open / choose in the menus), and scroll the list,
the preview or a menu with the wheel. **Dragging in the preview selects text and copies it on
release** — only the conversation, not the borders, gutters or the session list next to it,
with wrapped lines joined back together. Double-click a message to copy all of it. Copies go
through `wl-copy` (or `xclip`/`xsel`, or OSC 52). `mouse = false` in the config or
`--no-mouse` gives the mouse back to the terminal; most terminals also bypass the capture
while Shift is held.

In the folder browser: Enter opens a folder (or chooses **✓ start here**), → / ← go in and
up, Backspace on an empty filter goes up, Tab switches to the folders recent sessions ran
in, ^T shows hidden folders, ^S starts in the highlighted folder, and typing a path such
as `~/src/x` then Enter jumps there.

## Command line

```
seance [FILTER…]          open the picker (-a AGENT one tab, -H only this folder, -n new-session menu, --no-mouse)
seance list [-a AGENT] [-H] [-n N] [--json]
seance show ID [--copy]   print a session's conversation, or copy it
seance resume ID          resume by id (a unique prefix is enough)
seance new AGENT [DIR]    start a new session
seance agents             which agents are installed, with session counts
```

`--print` prints `cd DIR && COMMAND` instead of running it. The picker draws on stderr, so
this shell function resumes a session and leaves your shell in its folder:

```sh
sea() { local c; c=$(seance --print "$@") && eval "$c"; }
```

Without `--print`, seance replaces itself with the agent (`exec`) in the session's folder.

## Config

`~/.config/seance/config.toml`, all optional:

```toml
hide = ["sd"]                 # agents left out of the list and the new-session menu
preview = true                # show the preview pane on start
mouse = true                  # clicks, wheel and drag-to-copy; false leaves the mouse to the terminal

[agents.claude]
command = "claude"            # program to run
args = ["--model", "opus"]    # added to every launch, new or resumed
```

Warnings (an unreadable store, a failed launch) go to the system log: `journalctl -t seance`.

## Building

```
cargo install --path .        # ~/.cargo/bin/seance
packaging/rpm.sh [--install]  # RPM in target/rpmbuild/RPMS/
packaging/deb.sh              # .deb in target/deb/ (no dpkg needed to build it)
```

Licensed under the Apache License 2.0.
