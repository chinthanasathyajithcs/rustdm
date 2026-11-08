# rustdm Terminal-First Interface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the TUI rustdm's complete interface: screens, a dialog stack, shared input widgets, and daemon-kept defaults for new downloads.

**Architecture:** Keep today's single-state design: one `App`, terminal events turned into `Action`s, one runtime loop that talks to the daemon. Screens and dialogs only read and change `App` and return `Action`s; only `runtime.rs` and `daemon_link.rs` touch the daemon. Phase 1 restructures with no behaviour change (except `Esc`), phase 2 adds the daemon defaults, phase 3 adds the features.

**Tech Stack:** Rust 2021, tokio, ratatui 0.30.2 (crossterm backend, `TestBackend` in tests), serde, clap. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-09-rustdm-terminal-first-design.md`

## Global Constraints

- No new crates. Ask before adding any dependency.
- The daemon's JSON-RPC API stays additive: existing methods and fields keep their names and meaning; new fields are `#[serde(default)]` / `skip_serializing_if`.
- `QUEUE_VERSION` stays 1; a queue file written before this work must load unchanged.
- CLI subcommands (`rdm add`, `list`, `pause`, `resume`, `cancel`, `remove`, `move`, `speed`, `limits`, `download`, `daemon …`) keep working for scripts.
- Tests never write into the real Downloads folder: set the daemon's default folder or pass a destination inside a `tempfile::tempdir()`.
- Connections range: 1 to 16 (`engine::MAX_CONNECTIONS`). Downloads-at-once range stays 1 to 32 (`MAX_ACTIVE_LIMIT`).
- Copy is plain and specific; errors say what went wrong and how to fix it.
- Every source file stays under 800 lines; functions under about 50 lines.
- After each task: `cargo fmt --check`, `cargo clippy --all-targets --features testing -- -D warnings`, `cargo test --features testing` all pass.
- Commit messages: `<type>: <description>`, ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

- **Windows-style paste** (CRLF line endings, trailing spaces, tabs) into the URL field must give clean URLs with no stray `\r`. Test in Task 9.
- **Non-ASCII text** in any field (é, 日本語, emoji) must move the cursor by character and never panic on a byte boundary. Test in Task 8.
- **A large `rdmd.log`** (tens of MB) must not be read whole: Logs reads only the last 64 KiB. Test in Task 12.
- **A tiny terminal with a dialog open** (30×8) must render without panicking, the dialog clipped to the screen. Test in Task 14.
- **The daemon going away during a submit** must leave the dialog open with what was typed and a clear error. Test in Task 9.

---

## Phase 1: Restructure (no behaviour change except `Esc`)

### Task 1: Shared `units` and `dirs` modules

**Files:**
- Create: `src/units.rs`, `src/dirs.rs`
- Modify: `src/lib.rs` (add `pub mod units; pub mod dirs;`), `src/main.rs:535-557` (remove `parse_speed`, import it), `src/tui/mod.rs:269-277` (remove `default_downloads_dir`, re-export `pub use crate::dirs::default_downloads_dir;`)

**Interfaces:**
- Produces: `units::parse_speed(raw: &str) -> Result<u64, String>` (moved unchanged), `units::parse_count(raw: &str, min: u32, max: u32) -> Result<u32, String>`, `dirs::default_downloads_dir() -> PathBuf` (moved unchanged).

- [ ] **Step 1: Write failing tests in `src/units.rs`.** Move the existing `parse_speed` tests from `src/main.rs` (e.g. `parse_speed("2M") == Ok(2 * 1024 * 1024)`) and add:

```rust
#[test]
fn counts_accept_whole_numbers_in_range_only() {
    assert_eq!(parse_count("8", 1, 16), Ok(8));
    assert_eq!(parse_count(" 16 ", 1, 16), Ok(16));
    assert_eq!(parse_count("0", 1, 16), Err("Enter a whole number from 1 to 16".into()));
    assert_eq!(parse_count("2.5", 1, 16), Err("Enter a whole number from 1 to 16".into()));
}
```

- [ ] **Step 2: Run** `cargo test --features testing --lib units` and see it fail (`parse_count` not found).
- [ ] **Step 3: Implement** `parse_count` (trim, parse `u32`, check range, the exact error text above) and move `parse_speed` and `default_downloads_dir`.
- [ ] **Step 4: Run** `cargo test --features testing` and confirm everything passes.
- [ ] **Step 5: Commit** `refactor: move speed parsing and the Downloads folder into shared modules`.

### Task 2: Split TUI state, actions and the daemon link

**Files:**
- Create: `src/tui/state.rs` (from `app.rs`: `App`, `Tab`, `Button`, `Mode`, `HitMap`, `Notice`), `src/tui/action.rs` (`Action`), `src/tui/daemon_link.rs` (from `mod.rs`: `refresh`, `perform`, `listen_for_telemetry`), `src/tui/runtime.rs` (from `mod.rs`: `run_app`), `src/tui/screens/mod.rs`, `src/tui/screens/downloads.rs` (from `app.rs`: key and mouse handling)
- Delete: `src/tui/app.rs`
- Modify: `src/tui/mod.rs` (keeps `run_interactive`, `run_plain`, terminal setup, re-exports `run_app`)

**Interfaces:**
- Produces:
  - `screens::downloads::handle_event(app: &mut App, event: &Event) -> Option<Action>`: today's `App::handle_event`, unchanged.
  - `daemon_link::perform(client: &mut Option<DaemonClient>, action: &Action, downloads_dir: &Path) -> Result<Outcome, ActionError>`.
  - `pub struct Outcome { pub message: String, pub select: Option<String> }`.
  - `pub struct ActionError { pub message: String, pub code: Option<i32>, pub not_added: Vec<String> }` with `From<ClientError>` (`code` from `ClientError::Rpc(e).code`).
  - `App::apply_outcome(&mut self, result: Result<Outcome, ActionError>)`: today it only notifies and applies `select`; Tasks 9 and 11 extend it.
  - `runtime::run_app` keeps its current signature.

- [ ] **Step 1: Move the code and its tests** with `pub(crate)` where needed. The tests in `app.rs` move to `screens/downloads.rs` and `state.rs` beside the code they cover.
- [ ] **Step 2: Add a test in `state.rs`:**

```rust
#[test]
fn an_outcome_selects_the_new_download_and_an_error_becomes_a_notice() {
    let mut app = App::default();
    app.apply_outcome(Ok(Outcome { message: "Added dl_1 (queued)".into(), select: Some("dl_1".into()) }));
    assert_eq!(app.selected.as_deref(), Some("dl_1"));
    app.apply_outcome(Err(ActionError { message: "boom".into(), code: None, not_added: vec![] }));
    assert_eq!(app.notice, Some(Notice { text: "boom".into(), is_error: true }));
}
```

- [ ] **Step 3: Run** `cargo test --features testing` and confirm all existing TUI tests (unit and `tests/tui.rs`) pass unchanged, plus the new one.
- [ ] **Step 4: Commit** `refactor: split the TUI into state, actions, runtime and daemon link`.

### Task 3: Split rendering into widgets, the Downloads screen and a theme

**Files:**
- Create: `src/tui/theme.rs` (every `Color` and `Style` now inline in `ui.rs`, as named `pub const`s or `pub fn`s, e.g. `theme::status_style(DownloadStatus) -> Style`, `theme::BRAND`, `theme::ERROR`), `src/tui/widgets/mod.rs`, `widgets/table.rs` (`render_table`, `progress_bar`, `size_text`), `widgets/segment_map.rs` (`MapCell`, `segment_cells`, `render_segment_map`), `widgets/lanes.rs`, `widgets/speed_graph.rs`
- Modify: `src/tui/screens/downloads.rs` (gains `render(frame, app, area)`: header, toolbar, tabs, table, inspector, footer), `src/tui/screens/mod.rs` (gains `pub fn render(frame: &mut Frame, app: &mut App)`, which dispatches by screen; only Downloads exists yet)
- Delete: `src/tui/ui.rs`

**Interfaces:**
- Produces: `screens::render(frame: &mut Frame, app: &mut App)` replaces `ui::render`; `widgets::segment_map::segment_cells` keeps its signature; all styles come from `theme`.

- [ ] **Step 1: Move the code; the `ui.rs` tests move with it.** `grep -rn "Color::" src/tui --include=*.rs | grep -v theme.rs` must print nothing.
- [ ] **Step 2: Run** `cargo test --features testing` and confirm everything passes, including `dashboard_shows_table_details_and_records_click_targets` and `small_windows_still_render_without_panicking`.
- [ ] **Step 3: Commit** `refactor: split TUI rendering into widgets, screens and a theme`.

### Task 4: Screens, global keys and Help

**Files:**
- Modify: `src/tui/state.rs` (add `pub enum Screen { Downloads, Settings, Logs }`, fields `screen: Screen`, `help_open: bool`, `HitMap.screens: Vec<(Rect, Screen)>`), `src/tui/screens/mod.rs` (`pub fn handle_event(app: &mut App, event: &Event) -> Option<Action>` runs the global keys, then the current screen's handler)
- Create: `src/tui/screens/help.rs` (`pub fn keys(screen: Screen) -> &'static [(&'static str, &'static str)]`, and `render_overlay`), `src/tui/screens/settings.rs` and `screens/logs.rs` with `render` showing the screen's title and "Not built yet" (filled in Tasks 11 and 12)
- Test: unit tests in `screens/mod.rs`, `tests/tui.rs`

**Interfaces:**
- Produces: `Screen`; `help::keys(Screen)`, the one key table that both Help and the footer read (the footer shows the first entries that fit).
- Global keys from spec section 4: `s` Settings, `l` Logs, `?` toggles Help, `Esc` closes Help, otherwise returns to Downloads, otherwise does nothing; `q` and `Ctrl+C` give `Action::Quit`. `Q` is added in Task 10.

- [ ] **Step 1: Write the failing tests:** `esc_on_downloads_does_not_quit` (`handle_event(Esc) == None` and the screen is still Downloads); `s_opens_settings_and_esc_returns`; `l_opens_logs`; `question_mark_toggles_help_and_esc_closes_it`; `clicking_a_screen_name_switches_screen` (via `hits.screens`); `the_footer_lists_keys_for_the_current_screen` (render at 120×40 with `TestBackend`, assert the footer contains `s Settings` on Downloads and `Esc Back` on Settings); `the_header_says_reconnecting_while_rdmd_is_away` (`app.connected = false` → the header contains `rdmd reconnecting…`, replacing today's `rdmd not connected`). Change the existing test that expected `Esc` to quit so it expects `q`.
- [ ] **Step 2: Run** `cargo test --features testing --lib tui` and see the new tests fail.
- [ ] **Step 3: Implement.** The header line shows `Downloads · Settings · Logs` after the brand, with the current one highlighted, and records `hits.screens`. Help renders as a centred bordered box over the screen listing `help::keys(app.screen)`.
- [ ] **Step 4: Run** `cargo test --features testing` and confirm it passes.
- [ ] **Step 5: Commit** `feat: add TUI screens, global keys and a help overlay`.

## Phase 2: Daemon defaults

### Task 5: Defaults kept by the daemon

**Files:**
- Modify: `src/ipc/protocol.rs` (types), `src/daemon/queue_store.rs` (`QueueSnapshot.defaults`), `src/daemon/core.rs` (state, `get_defaults`, `set_defaults`, persistence), `src/daemon/server.rs` (dispatch), `src/ipc/client.rs` (client methods)
- Test: `tests/daemon_core.rs`, unit tests in `queue_store.rs` and `protocol.rs`

**Interfaces:**
- Produces:
  - `protocol::SavedDefaults { folder: Option<PathBuf>, connections: Option<u32>, max_bytes_per_sec: u64 }` (`Default`, serde).
  - `protocol::DownloadDefaults { folder: PathBuf, connections: u32, max_bytes_per_sec: u64 }`: resolved values.
  - `protocol::SetDefaultsParams { folder: Option<PathBuf>, connections: Option<u32>, max_bytes_per_sec: Option<u64> }`: `None` leaves a field unchanged.
  - `DaemonCore::get_defaults(&self) -> DownloadDefaults`, `DaemonCore::set_defaults(&self, SetDefaultsParams) -> Result<DownloadDefaults, RpcError>`.
  - RPC `daemon.get_defaults`, `daemon.set_defaults`.
  - `DaemonClient::get_defaults(&mut self) -> Result<DownloadDefaults, ClientError>`, `DaemonClient::set_defaults(&mut self, SetDefaultsParams) -> Result<DownloadDefaults, ClientError>`.
- Resolution: an unset folder → `dirs::default_downloads_dir()`; unset connections → `engine::DEFAULT_CONNECTIONS` (8).
- Validation, all `RpcError::invalid_params`:
  - connections outside 1..=16 → `"connections: must be between 1 and 16"`
  - a folder that isn't absolute → `"folder: must be a full path"`
  - a folder that exists but isn't a directory → `"folder: is a file, not a folder"`
  - an existing folder where creating and removing `.rdm-write-test-<pid>` fails → `"folder: can't write there ({io error})"`
  - an invalid speed → the existing `validate_speed` message prefixed with `"speed: "`

- [ ] **Step 1: Write the failing tests:** `defaults_start_as_the_downloads_folder_and_eight_connections`; `set_defaults_changes_only_the_fields_given`; `bad_defaults_are_rejected_with_the_field_named` (one assertion per message above, using a temp file for the "is a file" case); `defaults_survive_a_daemon_restart` (`with_store` on a temp dir, set, drop, reload, get); `a_queue_file_without_defaults_still_loads` (write today's snapshot JSON without `defaults`, load, defaults are the built-in ones).
- [ ] **Step 2: Run** `cargo test --features testing --test daemon_core` and see them fail.
- [ ] **Step 3: Implement.** Keep `SavedDefaults` in the core's state beside the limits, include it in the snapshot `persist()` writes, and add the dispatch arms next to `daemon.get_limits` and `daemon.set_limits`.
- [ ] **Step 4: Run** `cargo test --features testing` and confirm it passes.
- [ ] **Step 5: Commit** `feat: keep defaults for new downloads in the daemon`.

### Task 6: Apply the defaults when adding; `rdm defaults`

**Files:**
- Modify: `src/daemon/core.rs:209-262` (`add_download`), `src/main.rs` (`Add` handling around lines 395-410; new `Defaults` command; `format_defaults`), `src/tui/daemon_link.rs` (Add sends `destination: None`; `refresh` also loads `app.defaults`), `src/tui/state.rs` (field `defaults: Option<DownloadDefaults>`), `src/tui/runtime.rs` and `src/tui/mod.rs` (drop the `downloads_dir` parameter from `run_app`, `run_interactive` and `perform`), `tests/tui.rs`, `tests/cli_daemon.rs`
- Test: `tests/daemon_core.rs`, `tests/cli_daemon.rs`, `src/main.rs` unit tests

**Interfaces:**
- Consumes: Task 5's `get_defaults`, `set_defaults`, `DownloadDefaults`, `SetDefaultsParams`.
- Produces: `run_app(terminal: &mut Terminal<B>, events: mpsc::Receiver<Event>, endpoint: &IpcEndpoint) -> TuiResult<()>`; `run_interactive(endpoint: &IpcEndpoint) -> TuiResult<()>`; `perform(client, action: &Action) -> Result<Outcome, ActionError>`; `format_defaults(&DownloadDefaults) -> String`.
- `add_download` takes each empty field from the resolved defaults: `destination`, `connections`, `max_bytes_per_sec`.
- `rdm add` sends `destination` only when `--dir` was given, made absolute with `std::path::absolute` against the CLI's working folder.
- `rdm defaults [--dir PATH] [--connections N] [--speed SPEED]`: no options prints, options set; `--speed` uses `units::parse_speed`.

- [ ] **Step 1: Write the failing tests:**
  - `add_without_a_destination_uses_the_default_folder` (core test: set defaults to a temp dir, add, `summary.destination` equals it)
  - `add_takes_connections_and_speed_from_the_defaults`
  - `defaults_print_folder_connections_and_speed_in_words`:

```rust
assert_eq!(format_defaults(&DownloadDefaults { folder: "/dl".into(), connections: 8, max_bytes_per_sec: 0 }),
           "Folder: /dl\nConnections: 8\nSpeed cap per download: unlimited");
```

  - `rdm_defaults_sets_and_prints` (CLI integration in `tests/cli_daemon.rs`)
  - `rdm_add_without_dir_follows_the_default_folder`

  Update `tests/tui.rs`: `start_dashboard` no longer takes a folder; each test calls `client.set_defaults(SetDefaultsParams { folder: Some(dir), ..Default::default() })` first.
- [ ] **Step 2: Run** `cargo test --features testing` and see the new tests fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --features testing` and confirm it passes. Check that no test created anything in the real `~/Downloads`: `ls ~/Downloads` before and after shows no new files.
- [ ] **Step 5: Commit** `feat: apply daemon defaults to new downloads and add rdm defaults`.

### Task 7: `ping` reports the daemon's data folder and log

**Files:**
- Modify: `src/ipc/protocol.rs:397-400` (`PingResult`), `src/daemon/core.rs` (field `data_dir: Option<PathBuf>`, set in `with_store` from `store.path().parent()`), `src/daemon/server.rs:29-37`
- Test: `tests/daemon_core.rs`, `protocol.rs` unit test

**Interfaces:**
- Produces: `PingResult { version, uptime_secs, data_dir: Option<PathBuf>, log_path: Option<PathBuf> }`, both new fields `#[serde(default, skip_serializing_if = "Option::is_none")]`. `log_path = daemon::log::log_path(data_dir)`.

- [ ] **Step 1: Write the failing tests:** `ping_reports_the_log_path_of_a_daemon_with_a_data_folder` (`with_store` in a temp dir → `log_path == Some(<dir>/rdmd.log)`); `ping_without_a_data_folder_reports_none`; `an_old_ping_reply_without_paths_still_decodes`.
- [ ] **Step 2: Run** and see them fail. **Step 3: Implement.** **Step 4: Run** `cargo test --features testing` and confirm it passes.
- [ ] **Step 5: Commit** `feat: report the daemon's data folder and log path in ping`.

## Phase 3: Features

### Task 8: Text field widget

**Files:**
- Create: `src/tui/widgets/text_field.rs`
- Test: unit tests in the same file

**Interfaces:**
- Produces:
  - `pub struct TextField { text: String, cursor: usize }`, where `cursor` counts characters, not bytes.
  - `TextField::new(text: &str) -> Self`, with the cursor at the end.
  - `text(&self) -> &str`.
  - `handle_key(&mut self, key: &KeyEvent) -> bool`: true if the key was used.
  - `paste(&mut self, text: &str)`.
  - `render(&self, frame: &mut Frame, area: Rect, focused: bool)`.
- Keys: printable characters insert; `←` `→` `Home` `End` move; `Backspace` and `Delete` remove; `Ctrl+U` clears. When the text is wider than the area, it scrolls so the cursor stays visible.

- [ ] **Step 1: Write the failing tests:** `typing_inserts_at_the_cursor`; `home_end_and_arrows_move_the_cursor`; `backspace_and_delete_remove_around_the_cursor`; `ctrl_u_clears`; `paste_inserts_at_the_cursor`; `cursor_moves_by_character_in_non_ascii_text` (`"日本é🎉"`: `End`, `Backspace` → `"日本é"`, `Home`, `Delete` → `"本é"`, no panic); `long_text_scrolls_to_keep_the_cursor_visible` (render 10 columns wide on `TestBackend`, the last characters are visible).
- [ ] **Step 2: Run** and see them fail. **Step 3: Implement.** **Step 4: Run** and confirm it passes.
- [ ] **Step 5: Commit** `feat: add a text field widget for the TUI`.

### Task 9: Dialog stack and the Add dialog

**Files:**
- Create: `src/tui/dialogs/mod.rs`, `src/tui/dialogs/add.rs`
- Modify: `src/tui/state.rs` (field `dialogs: Vec<Dialog>`; remove `Mode::AddUrl`; `apply_outcome` routes to a pending dialog), `src/tui/action.rs` (`Add(Vec<AddDownloadParams>)` replaces `Add(String)`), `src/tui/daemon_link.rs` (multi-URL add), `src/tui/screens/mod.rs` (the top dialog gets events first; dialogs render last over a dimmed screen), `src/tui/screens/downloads.rs` (`a` and the Add button push the dialog; a paste while browsing opens it with the paste)
- Test: unit tests in `dialogs/add.rs` and `state.rs`; `tests/tui.rs`

**Interfaces:**
- Consumes: Task 8's `TextField`; Task 1's `parse_speed` and `parse_count`; Task 6's `app.defaults`.
- Produces:
  - `pub enum Dialog { Add(AddDialog) }`; Task 10 adds `Confirm` and `Speed`.
  - `Dialog::handle_event(&mut self, event: &Event) -> DialogResult`, `Dialog::render(&self, frame, area)`, `Dialog::set_error(&mut self, err: &ActionError)`, `Dialog::is_pending(&self) -> bool`.
  - `pub enum DialogResult { Used, Close, Submit(Action) }`. `Submit` marks the dialog pending.
  - `AddDialog::new(defaults: Option<&DownloadDefaults>, paste: Option<&str>) -> Self`.
  - `pub fn parse_urls(text: &str) -> Vec<String>`: split on `\n`, trim whitespace including `\r`, drop blank lines.
- Behaviour:
  - Fields in order: URL, Folder, File name, Connections, Speed cap. Folder, connections and speed are pre-filled from the defaults.
  - `Enter` in any field submits.
  - Validation before submitting: any URL whose scheme isn't `http` or `https`, or that doesn't parse, gives `"Only http and https links can be downloaded"` under URL. A bad connections value gives the `parse_count(…, 1, 16)` message under Connections; a bad speed gives the `parse_speed` message under Speed cap.
  - A blank file name sends `filename: None`. With several URLs, the file name is ignored and the field shows "Each file keeps the server's name".
  - `perform` for `Add` adds each URL in turn. On any failure it returns `ActionError { not_added: <urls that failed>, message: "<n> of <m> not added: <first error>" }`.
  - `apply_outcome(Err)` with a pending dialog calls `set_error` and clears pending. `AddDialog::set_error` replaces the URL list with `not_added` when it's non-empty, puts errors with code `CODE_INVALID_PATH`, `CODE_PERMISSION_DENIED` or `CODE_DESTINATION_EXISTS` under Folder, and everything else under the title.
  - `apply_outcome(Ok)` with a pending dialog closes it, then notifies and selects as before.
  - Mouse: `Dialog::render` records each field's and button's area in `app.hits.dialog: Vec<(Rect, DialogTarget)>`; a click on a field focuses it, a click on a button acts as its key.

- [ ] **Step 1: Write the failing tests:**
  - `a_paste_then_enter_submits_with_the_defaults` (the `Submit(Add(params))` has the default folder, connections and speed)
  - `pasted_crlf_lines_become_clean_urls`:

```rust
assert_eq!(parse_urls("https://a/1\r\n  https://a/2 \r\n\r\n\thttps://a/3\t"),
           vec!["https://a/1", "https://a/2", "https://a/3"]);
```

  - `a_non_http_url_is_rejected_under_the_url_field`
  - `a_bad_connections_value_is_rejected_under_its_field`
  - `a_failed_submit_keeps_the_typed_values` (submit, then `apply_outcome(Err(ActionError::from(ClientError::NotRunning)))`: the dialog is still open, the fields are unchanged, the title error is `"rdmd is not running"`)
  - `only_the_urls_that_failed_stay_in_the_dialog`
  - `esc_closes_the_dialog_without_adding`
  - `clicking_a_field_focuses_it`
  - `letters_typed_into_the_dialog_are_not_shortcuts` (moved from today's test)
  - e2e in `tests/tui.rs`: `the_add_dialog_adds_with_the_daemon_defaults` (set defaults to a temp folder with 2 connections; `a`, paste the mock server URL, `Enter`; wait for completion; the file is in that folder)
- [ ] **Step 2: Run** and see them fail. **Step 3: Implement.** **Step 4: Run** `cargo test --features testing` and confirm it passes.
- [ ] **Step 5: Commit** `feat: add a dialog stack and a full Add dialog to the TUI`.

### Task 10: Cancel, Speed and Stop-daemon dialogs

**Files:**
- Create: `src/tui/dialogs/confirm.rs`, `src/tui/dialogs/speed.rs`
- Modify: `src/tui/dialogs/mod.rs` (variants), `src/tui/state.rs` (remove `Mode::ConfirmCancel`, and `Mode` itself once it's empty; `Button::Speed`), `src/tui/action.rs`, `src/tui/daemon_link.rs`, `src/tui/screens/downloads.rs` (`c`, `v`, the Speed button), `src/tui/screens/mod.rs` (global `Q`), `src/tui/screens/help.rs` (keys)
- Test: unit tests in the new files; `tests/tui.rs`

**Interfaces:**
- Consumes: Task 9's `Dialog`, `DialogResult`, `TextField`.
- Produces: `Action::Cancel { id: String, delete_files: bool }` (replaces `Cancel(String)`), `Action::SetSpeed { id: String, max_bytes_per_sec: u64 }`, `Action::StopDaemon`; `ConfirmDialog::cancel(summary: &DownloadSummary)`, `ConfirmDialog::stop_daemon(active: usize)`, `SpeedDialog::new(summary: &DownloadSummary)`.
- Exact copy:
  - Cancel: `"Cancel {id} ({filename})?"` with `y Cancel  d Cancel and delete partial files  n Keep it`.
  - Stop: `"Stop rdmd? {n} downloads will pause and continue the next time it starts."` (`"1 download"` for one) with `y Stop  n Keep running`.
  - Speed: title `"Speed cap for {filename}"`, the field pre-filled with `format_speed_cap(max_bytes_per_sec)`, hint `"500K, 2M, or off"`.
- `perform`: `Cancel` → `cancel(&id, delete_files)`; `SetSpeed` → `set_download_speed`; `StopDaemon` → `shutdown`, then `Outcome { message: "rdmd stopped" }` and the runtime quits the TUI.

- [ ] **Step 1: Write the failing tests:** `c_asks_and_y_cancels_keeping_files`; `d_cancels_and_deletes_partial_files`; `n_or_esc_keeps_the_download`; `clicking_a_choice_picks_it` (a click on `d Cancel and delete partial files` submits `Cancel { delete_files: true }`); `v_opens_the_speed_dialog_prefilled_with_the_current_cap`; `off_removes_the_speed_cap`; `a_bad_speed_shows_the_parse_error_under_the_field`; `capital_q_asks_before_stopping_and_counts_active_downloads` (2 active → the text says "2 downloads"); e2e `stopping_the_daemon_from_the_tui_pauses_downloads` (start a throttled download, `Q`, `y`; `run_app` returns; `DaemonClient::connect` reports `NotRunning`).
- [ ] **Step 2: Run** and see them fail. **Step 3: Implement.** **Step 4: Run** `cargo test --features testing` and confirm it passes.
- [ ] **Step 5: Commit** `feat: add cancel, speed cap and stop-daemon dialogs to the TUI`.

### Task 11: Settings screen

**Files:**
- Modify: `src/tui/screens/settings.rs` (replaces the placeholder), `src/tui/state.rs` (`settings: SettingsState`; `apply_outcome` routes to settings while it's pending), `src/tui/action.rs` (`SetLimits(SetLimitsParams)`, `SetDefaults(SetDefaultsParams)`), `src/tui/daemon_link.rs`
- Test: unit tests in `settings.rs`; `tests/tui.rs`

**Interfaces:**
- Consumes: Task 8's `TextField`; Task 5's `SetDefaultsParams`; the existing `SetLimitsParams`.
- Produces:
  - `pub enum SettingsField { MaxActive, TotalSpeed, Folder, Connections, Speed }`, in that order.
  - `pub struct SettingsState { selected: SettingsField, editing: Option<TextField>, error: Option<String>, pending: bool }`.
- Behaviour:
  - Two groups, "Limits" (downloads at once, total speed cap) and "New downloads" (folder, connections, speed cap per download), with values from `app.limits` and `app.defaults`.
  - `↑` `↓` select a field. `Enter` starts editing, pre-filled with the current value. `Enter` again validates (`parse_count(…, 1, 32)` for downloads at once, `parse_count(…, 1, 16)` for connections, `parse_speed` for speeds) and submits one-field params. `Esc` while editing reverts.
  - An error shows under the field being edited; success ends editing and refreshes the values.
  - Mouse: clicking a field selects it; clicking a selected field starts editing.

- [ ] **Step 1: Write the failing tests:** `enter_edits_and_saves_one_field` (Folder → `Submit(SetDefaults { folder: Some(..), connections: None, max_bytes_per_sec: None })`); `esc_while_editing_reverts`; `a_daemon_rejection_shows_under_the_field` (`apply_outcome(Err(ActionError { message: "folder: must be a full path".into(), code: Some(CODE_INVALID_PARAMS), not_added: vec![] }))` → the error is shown and editing continues); `clicking_a_field_selects_it`; `limits_reject_zero_downloads_at_once`; e2e `settings_saves_the_default_folder_to_the_daemon` (edit Folder, then `client.get_defaults().folder` equals the new path).
- [ ] **Step 2: Run** and see them fail. **Step 3: Implement.** **Step 4: Run** `cargo test --features testing` and confirm it passes.
- [ ] **Step 5: Commit** `feat: add a Settings screen for limits and download defaults`.

### Task 12: Logs screen

**Files:**
- Modify: `src/tui/screens/logs.rs` (replaces the placeholder), `src/tui/state.rs` (`logs: LogsState`; `history: VecDeque<Notice>`, capped at `NOTICE_HISTORY = 200`, filled by `notify`; `log_path: Option<PathBuf>`), `src/tui/runtime.rs` (on connect, call `ping` and store `log_path`; while on Logs, reread the tail every `REFRESH_INTERVAL`)
- Test: unit tests in `logs.rs`

**Interfaces:**
- Consumes: Task 7's `PingResult.log_path`.
- Produces:
  - `pub const LOG_TAIL_BYTES: u64 = 64 * 1024;`
  - `pub fn read_tail(path: &Path, max_bytes: u64) -> io::Result<Vec<String>>`: seek to `len - max_bytes` when the file is larger, drop the first, partial line, and split the rest into lines with lossy UTF-8.
  - `pub struct LogsState { lines: Vec<String>, scroll: usize, error: Option<String> }`: `scroll` counts lines up from the newest.
- Behaviour:
  - The top area shows the log lines; the bottom area (at most 6 rows) shows `history`, newest last.
  - `↑` `↓` scroll by a line, `PgUp` `PgDn` by a page, `G` returns to the newest line. The mouse wheel scrolls.
  - A missing file shows `"No log yet at {path}"`. Any other error shows `"Can't read {path}: {error}"`. No `log_path` (an old daemon) shows `"This rdmd doesn't report where its log is"`.

- [ ] **Step 1: Write the failing tests:** `read_tail_returns_whole_small_files`; `only_the_last_64_kib_of_a_large_log_is_read` (write about 5 MB of numbered lines; the result is at most 64 KiB, ends with the last line and starts on a line boundary); `invalid_utf8_is_shown_not_fatal`; `a_missing_log_says_where_it_will_be`; `g_jumps_back_to_the_newest_line`; `notices_are_kept_in_history_up_to_the_cap`.
- [ ] **Step 2: Run** and see them fail. **Step 3: Implement.** **Step 4: Run** and confirm it passes.
- [ ] **Step 5: Commit** `feat: add a Logs screen with the daemon log and notice history`.

### Task 13: Open the folder of a download

**Files:**
- Create: `src/tui/open_folder.rs`
- Modify: `src/tui/action.rs` (`OpenFolder(PathBuf)`), `src/tui/screens/downloads.rs` (`o`), `src/tui/daemon_link.rs` (`perform` runs it without the daemon), `src/tui/screens/help.rs`
- Test: unit tests in `open_folder.rs` and `downloads.rs`

**Interfaces:**
- Produces:
  - `pub fn open_command(path: &Path, file_exists: bool) -> (OsString, Vec<OsString>)`, chosen by `cfg`:
    - Windows: `explorer` with `/select,<path>` when the file exists, else the folder
    - Linux and other Unix: `xdg-open <folder>`
    - macOS: `open -R <path>` when it exists, else `open <folder>`
  - `pub fn open_folder(path: &Path) -> io::Result<()>`: spawns that command with null stdio and doesn't wait.
- `o` on a download sends `OpenFolder(destination.join(filename))`. With no selection it gives the notice `"Select a download first"`. A failure gives `"Couldn't open the folder: {command} failed: {error}"`.

- [ ] **Step 1: Write the failing tests:** `open_command_selects_an_existing_file` and `open_command_opens_the_folder_when_the_file_is_gone` (assert on this platform's branch, `#[cfg]`-gated); `o_targets_the_selected_download_path`.
- [ ] **Step 2: Run** and see them fail. **Step 3: Implement.** **Step 4: Run** and confirm it passes.
- [ ] **Step 5: Commit** `feat: open a download's folder from the TUI`.

### Task 14: Small terminals

**Files:**
- Modify: `src/tui/screens/downloads.rs` (inspector rule), `src/tui/widgets/table.rs` (column rule), `src/tui/dialogs/mod.rs` (dialog sizing)
- Test: rendering tests in those files

**Interfaces:**
- Produces:
  - `pub const INSPECTOR_MIN_ROWS: u16 = 24;`: the inspector shows only when the terminal height is at least this.
  - `pub fn table_columns(width: u16) -> Vec<Column>`: below 80 columns ETA drops; below 64, Speed drops too.
  - `dialogs::dialog_area(screen: Rect, wanted_width: u16, wanted_height: u16) -> Rect`: centred, clamped to the screen.

- [ ] **Step 1: Write the failing tests:** `the_inspector_hides_below_24_rows`; `eta_drops_below_80_columns_and_speed_below_64`; `dialogs_fit_tiny_terminals_without_panicking` (render every dialog and the Help overlay at 30×8 and 10×3); `long_urls_and_file_names_are_cut_not_wrapped` (a 300-character file name in a 80×24 table row stays on one line).
- [ ] **Step 2: Run** and see them fail. **Step 3: Implement.** **Step 4: Run** `cargo test --features testing` and confirm it passes.
- [ ] **Step 5: Commit** `feat: keep the TUI usable in small terminals`.

## After Phase 3

Phase 4 (polish: the visual design built on `theme.rs`) gets its own short design in chat once these tasks land, so the look is chosen against the real screens. Update the README's "Dashboard" section and the progress page when the branch is merged.
