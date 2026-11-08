# rustdm: terminal-first interface

Status: design approved in conversation, awaiting spec review. Date: 2026-11-09.

## 1. Intent

rustdm becomes a terminal application. The TUI (`rdm`, with no command) is the
interface people use, and the daemon (`rdmd`) does the work in the background.
Everything a person can do with rustdm can be done from the TUI, without typing
CLI commands.

Decisions made while designing:

- **CLI subcommands stay**, as a scripting layer over the same daemon calls.
  `rdm add`, `rdm list`, `rdm limits` and the rest keep working.
- **The desktop GUI planned for Milestone 3 is dropped for now.** A GUI may come
  later, so the daemon's JSON-RPC API stays the stable interface any front end
  sits on. Nothing in this design may make the TUI the only possible client.
- **Quitting the TUI leaves downloads running.** `q` closes the TUI; `Q` stops
  the daemon after a confirmation. Background downloads are the point of the
  daemon.
- **The name stays rustdm**, with the `rdm` and `rdmd` commands.
- **Approach:** restructure the TUI into screens, a dialog stack and shared
  widgets, keeping today's single-state, message-passing design (one `App`, one
  `Action` type), then add the missing features through that structure, then
  polish the look.

Success means all of these hold:

- Every action the CLI offers on daemon downloads is reachable from the TUI:
  add with folder, name, connections and speed cap; pause; resume; cancel, with
  or without deleting partial files; remove; reorder; per-download speed cap;
  global limits; defaults for new downloads; stopping the daemon.
- `a`, paste, `Enter` adds a download with the defaults.
- A rejected input never loses what the user typed: the dialog stays open with
  the error under the field.
- The TUI stays usable at 80×24 and degrades cleanly below it.
- All existing tests keep passing, and coverage stays at 80% or above.

Out of scope: the desktop GUI, browser capture, HLS/DASH, release packaging,
key rebinding, themes chosen by the user, copying to the clipboard, and
resuming a `.part.rdm` state file from the TUI (it stays a CLI action).

## 2. Screens and layout

Screen names in the header are clickable, and each has a key. A one-line footer
always shows the most useful keys for the current screen.

**Downloads** (default) keeps today's layout:

- Header: `rustdm`, daemon status (connected or reconnecting), global limits,
  total speed.
- Toolbar: Add · Pause · Resume · Cancel · Remove · Speed. Speed is new.
- Filter tabs: All · Active · Queued · Completed · Stopped.
- Downloads table, with the inspector below it: segment map, connection lanes,
  speed graph.

**Settings** (`s`) has two groups, each field edited in place and saved straight
to the daemon:

- Limits: downloads at once, total speed cap.
- New downloads: default folder, default connections, default per-download
  speed cap.

**Logs** (`l`) shows the end of `rdmd`'s log file, read directly from disk (the
TUI runs on the same machine as the same user), and below it the TUI's own
notice history, so an error that flashed by in the footer can be read again.

**Help** (`?`) is an overlay listing every key for the current screen.

**Small terminals:** below 24 rows the inspector hides. Below 80 columns the
table drops its least important columns, ETA first and then speed, instead of
wrapping.

## 3. Dialogs

Dialogs open as a centred box over the current screen, which dims, and they can
stack. Only the top dialog gets keys. `Tab` and `Shift+Tab` move between
fields, `Enter` submits, `Esc` closes, and the mouse can click fields and
buttons.

**Shared input widgets**, used by every dialog and by Settings:

- Text field: cursor, `←` `→` `Home` `End`, `Backspace` `Delete`, `Ctrl+U`
  clears, bracketed paste.
- Speed field: accepts `500K`, `2M`, `off` or `0`. It uses the same parser as the
  CLI, moved into the library.
- Number field: a whole number within a minimum and a maximum.

**Add download** (`a`):

- Fields: URL, then Folder, File name, Connections, Speed cap. The last four
  start from the defaults in Settings; a blank file name means the server's.
- `Enter` in the URL field submits with the other fields as they are.
- Pasting more than one line counts as several URLs. The field shows "3 URLs",
  and each is added with the same options, one `download.add` call per URL.
  Blank lines are ignored.
- A URL that is not `http` or `https` shows an error under the field, and the
  dialog stays open.

**Cancel** (`c`): "Cancel dl_3 (ubuntu.iso)?" with `y` (cancel), `d` (cancel and
delete the partial files) and `n` (keep it).

**Speed cap** (Speed button or `v`): one speed field, pre-filled with the
selected download's cap. `off` removes the cap.

**Stop the daemon** (`Q`): "Stop rdmd? N downloads will pause and continue the
next time it starts." with `y` and `n`. N is the number of active downloads.

**Remove** (`x`) has no dialog: it keeps the files and only takes the entry off
the list.

**Errors:** when the daemon rejects a submission, the dialog stays open with the
error under the field it concerns, or under the dialog's title when no single
field is at fault.

**Settings editing** works the same way: `Enter` starts editing the selected
field, `Enter` saves it, `Esc` reverts it.

## 4. Keys

Today's keys keep their meaning, with one exception, noted below.

**Every screen**

| Key | Does |
|---|---|
| `s` · `l` · `?` | Settings · Logs · Help |
| `Esc` | Back to Downloads; in a dialog, close it |
| `q`, `Ctrl+C` | Quit the TUI; downloads keep going |
| `Q` | Stop the daemon, after confirming |

**Downloads**

| Key | Does |
|---|---|
| `↑` `↓`, `j` `k`, `PgUp` `PgDn`, `Home` `End` | Move the selection |
| `Shift+↑` `Shift+↓`, `K` `J` | Move the selected download in the queue |
| `1`–`5`, `Tab` / `Shift+Tab`, `←` `→` | Switch filter tab |
| `a` · `p` · `r` · `c` · `x` | Add · Pause · Resume · Cancel · Remove |
| `v` | Speed cap for the selected download (new) |
| `o` | Open the folder holding the selected file (new): Explorer on Windows, selecting the file; `xdg-open` on Linux |

**Settings and Logs:** `↑` `↓` move between fields or lines; `Enter` edits a
field. On Logs, `PgUp` `PgDn` scroll and `G` jumps to the newest line.

**Mouse:** clicking buttons, tabs, rows, fields and screen names; the wheel
scrolls the table and the log.

**Change:** today `Esc` on the Downloads screen quits. It will do nothing there,
since `Esc` means "back" everywhere else. Quitting is `q` or `Ctrl+C`.

## 5. Daemon changes

Both are additive: old queue files load, and old clients keep working.

**Defaults for new downloads**

- New saved type `SavedDefaults { folder: Option<PathBuf>, connections:
  Option<u32>, max_bytes_per_sec: u64 }`, stored in the queue snapshot next to
  the limits with `#[serde(default)]`, so a file without it loads with built-in
  defaults.
- New methods `daemon.get_defaults` and `daemon.set_defaults`, mirroring the
  existing `daemon.get_limits` and `daemon.set_limits`. `get_defaults` returns
  the resolved values (`DownloadDefaults { folder: PathBuf, connections: u32,
  max_bytes_per_sec: u64 }`), so every client shows the real folder. The daemon
  validates: connections 1 to 16 (the engine's `MAX_CONNECTIONS`), a folder
  that is an absolute path and writable, a valid speed. A bad value returns
  `invalid params` whose message starts with the field name (`folder: `,
  `connections: `, `speed: `).
- `download.add` fills any field left empty from the defaults. An unset default
  folder means the user's Downloads folder, resolved by the daemon. (Today an
  empty destination falls back to the daemon's working folder; this replaces
  that.)
- `rdm add` stops resolving the Downloads folder itself and sends only what the
  user gave, so the CLI and the TUI follow the same Settings.
- New CLI command `rdm defaults [--dir] [--connections] [--speed]`, working like
  `rdm limits`: no options prints the defaults, options change them.

**`ping` reports the daemon's files**

- `PingResult` gains optional `data_dir` and `log_path` fields. The Logs screen
  uses `log_path`, so it finds `rdmd.log` even when the daemon runs with a
  custom data folder.

**No change needed** for adding several URLs (one `download.add` per URL), open
folder (built from the summary's `destination` and `filename`), stopping the
daemon (`daemon.shutdown` already pauses active downloads and marks them to
continue after a restart), cancel with delete, per-download speed caps and
reordering.

## 6. Code layout

Under `src/tui/`, one job per file:

| File | Job |
|---|---|
| `mod.rs` | Entry points (`run_interactive`, `run_plain`), terminal setup and restore |
| `runtime.rs` | The event loop over terminal events, telemetry and the refresh timer |
| `daemon_link.rs` | Refresh, performing actions, reconnecting, the telemetry listener |
| `state.rs` | `App`: shared daemon data, current screen, dialog stack, notice history, hit map |
| `action.rs` | `Action`: daemon calls, quit, stop daemon, open folder |
| `screens/` | `downloads.rs`, `settings.rs`, `logs.rs`, `help.rs`, each with `handle_event` and `render` |
| `dialogs/` | `mod.rs` (the stack), `add.rs`, `confirm.rs`, `speed.rs` |
| `widgets/` | Text, speed and number fields; segment map, connection lanes, speed graph, downloads table |
| `theme.rs` | Every colour and style |
| `format.rs` | Unchanged |

Screens and dialogs read and change `App` and return an `Action`; only
`runtime.rs` and `daemon_link.rs` talk to the daemon. That keeps every screen
testable without a daemon.

The CLI's speed parser moves from `src/main.rs` into the library (`src/units.rs`).
Opening a folder lives in `src/tui/open_folder.rs`, behind one function, with
the platform command chosen by `cfg`.

## 7. Errors

- **Daemon lost:** the header shows "reconnecting…", actions report that `rdmd`
  is not reachable, and the TUI reconnects on its own, as it does today.
- **Rejected submission:** the dialog stays open with the error (section 3).
- **No log file yet:** Logs says "No log yet at <path>".
- **Log file unreadable:** Logs shows the I/O error and the path.
- **`o` fails:** an error notice naming the command that failed.

## 8. Testing

Each test is written first and seen to fail.

- **Unit:** input widgets (editing, paste, speed parsing); each dialog's key
  handling (validation, multi-URL paste, error placement); each screen's keys.
- **Rendering:** every screen and dialog on ratatui's `TestBackend` at 80×24
  and 120×40; the inspector hidden below 24 rows; columns dropped below 80.
- **Daemon:** `defaults.get` and `defaults.set`; validation errors; defaults
  surviving a restart; `download.add` filling empty fields from the defaults;
  an old queue file without defaults still loading; `ping` reporting the log
  path.
- **CLI:** `rdm defaults` printing and setting; `rdm add` without `--dir`
  following the default folder.
- **End to end:** extend the TUI tests that drive `run_app` with scripted events
  against a real daemon: add with defaults, a Settings save, `Q` stopping the
  daemon, `Esc` no longer quitting.
- All current tests (232) stay green; coverage stays at 80% or above.

## 9. Build order

1. **Restructure.** Move today's TUI into the layout of section 6 with no
   change in behaviour except `Esc` on Downloads.
2. **Daemon.** `DownloadDefaults`, `defaults.get` and `defaults.set`, defaults
   applied in `download.add`, the `ping` fields, `rdm defaults`, `rdm add`
   sending only what the user gave.
3. **Features.** Add dialog, speed dialog, cancel with delete, Settings, Logs,
   Help, `Q`, `o`, small-terminal behaviour.
4. **Polish.** `theme.rs` and the visual design.
