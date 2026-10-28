# OatsNotes: security and privacy review (read-only), 2026-10-28, tree at 881d433

## Verdict

**Nothing in OatsNotes phones home on its own.** There is no HTTP, socket or URL-opening code in the workspace, and no networking crate in the normal dependency graph (269 crates; no reqwest, ureq, hyper, curl, rustls, tokio, webbrowser or telemetry SDK). The only runtime network paths are the two allowed ones:
- (a) FluidAudio downloads models from `https://huggingface.co`, and only when the model files are missing.
- (b) The app runs the `claude` CLI as a child process, and the CLI talks to Anthropic.

Two runs with the null engine showed no sockets at all (see "Dynamic check" below). Audio is never written to disk. Transcript text leaves the process only on the `claude` child's stdin.

The biggest gaps are not about phoning home:
1. The `claude` child inherits the app's **entire environment**. Inherited `ANTHROPIC_BASE_URL` or proxy variables could send transcripts to a host other than Anthropic, and the env-var names show `CLAUDE_CODE_*` session and messaging variables are currently present and would be passed on.
2. **Hotword commands have no budget or rate cap**, and the command path has no output bounds.
3. A model-controlled integer can **panic the brain thread**.

Smaller items cover file permissions, info-level logging of dictated text, and irreversible review/reorganise actions.

Build-time note: `fluidaudio-rs`'s build.rs runs `swift build`, which clones `github.com/FluidInference/FluidAudio` pinned to tag 0.14.8 / rev `56607d90…`. That is network access at build time, not at run time.

---

## Findings, ranked by severity

### 1. MEDIUM: the `claude` child inherits the whole environment (CONFIRMED)
**Evidence:** `crates/oats-brain/src/claude_cli.rs:237-255`. `command()` only does `env_remove("CLAUDECODE")` and sets `CLAUDE_CODE_ENTRYPOINT=oats-notes`. Everything else passes through.
- `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`, `HTTPS_PROXY` or `NODE_OPTIONS` in the launching shell would redirect or alter where the transcript prompt goes. `--setting-sources ""` stops `settings.json` `env` blocks, but not the real process environment.
- Launched from inside Claude Code, the child also gets `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_PID` and `CLAUDE_CODE_EXECPATH`. All of these were present in this review's own environment (names only). They could tie the brain's calls to the parent session.
- Unrelated secrets (tokens, keys) are handed to a network-facing process.

**Fix** (`ClaudeCli::command`):
```rust
const PASS: &[&str] = &["HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
                        "CLAUDE_CONFIG_DIR"];
cmd.env_clear();
for k in PASS { if let Some(v) = std::env::var_os(k) { cmd.env(k, v); } }
cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")          // plus the claude binary's own dir if needed
   .env("CLAUDE_CODE_ENTRYPOINT", "oats-notes")
   .env("DISABLE_TELEMETRY", "1")
   .env("DISABLE_ERROR_REPORTING", "1")                 // no Sentry reports that could carry prompt fragments
   .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1") // telemetry, error reporting, autoupdate
   .env("DISABLE_AUTOUPDATER", "1");
// opt-in only, e.g. OATS_CLAUDE_PASS_PROXY=1: HTTPS_PROXY, HTTP_PROXY, NO_PROXY, NODE_EXTRA_CA_CERTS
```
Auth uses the macOS Keychain, so `HOME` and `USER` are enough. Test once with the real CLI (`interpret_demo`) after the change. Add a unit test with the fake CLI that dumps `env` and asserts that `ANTHROPIC_BASE_URL` and `CLAUDE_CODE_SESSION_ID` are absent.

### 2. MEDIUM: hotword commands have no budget, rate limit or output bounds (CONFIRMED)
**Evidence:**
- `scheduler.rs:598-601`: `within_budget` returns `true` for `Tier::Reactive`. The hotword path (`scheduler.rs:721-760`) never checks the daily budget or the 20-runs/hour cap. Categorisation (tier 0) doesn't either.
- `hotword.rs:144-171`: the command schema has no `maxItems` on `actions` and no `maxLength` on strings.
- `hotword.rs:405-447`: `create_project` is not limited by `max_new_nodes_per_run`. That limit is enforced only in `interpret::resolve_node`, `interpret.rs:365,393`.

**Blast radius of overheard speech** (TV or a colleague saying "hey computer, …"):
- Unlimited haiku calls, each up to $0.50. Each call sends the full node tree plus the overheard text to Anthropic.
- Arbitrary numbers of events, reminders, notes and projects get created.
- **No destructive intent is reachable.** The enum is `add_event | add_reminder | add_note | create_project | unknown`, there is no delete/merge/archive, and every row is tagged `source=hotword` plus the segment id, so it can be audited.

**Fix:**
- Count tier 0 against the hourly cap, e.g. a separate `max_commands_per_hour = 10`, plus a daily ceiling.
- Debounce identical command text within 60 s.
- Add `"maxItems": 5` on `actions` and `"maxLength"` 200 on title/text/details/names (500 for details). Truncate again in `execute` regardless (`chars().take(n)`).
- Apply `max_new_nodes_per_run` (or 1) to `create_project`.
- Optional: require the hotword at the start of a segment, not anywhere in it (`detect_in` matches at any position).

### 3. MEDIUM/LOW: the model can panic the brain thread through `offset_days` (CONFIRMED)
**Evidence:**
- `hotword.rs:127` deserialises `offset_days: Option<i64>` with no bounds.
- `hotword.rs:326` → `dates::shift_days` → `chrono::Duration::days(days)` (`dates.rs:43`). In chrono 0.4.45 this is `expect(TimeDelta::try_days(days), "TimeDelta::days out of bounds")` (`time_delta.rs:137-138`), so it **panics** for |days| > about 1e14.
- There is no `catch_unwind` in `scheduler.rs`, so the brain worker dies silently until restart.

**Fix:**
- In `shift_days`, use `chrono::TimeDelta::try_days(days)?`.
- Clamp `offset_days` to `-366..=366` in `execute`.
- Add `"minimum": -366, "maximum": 366` to the schema.
- Defence in depth: wrap each job in `std::panic::catch_unwind(AssertUnwindSafe(..))` in `run_job`, and record the panic as a failed run.

### 4. LOW/MEDIUM: review and reorganise can make irreversible changes driven by transcript content (CONFIRMED)
- `interpret.rs:888-903`: `drop_event` deletes any event whose source isn't `Hotword`, so `Origin::User` events (added by hand in the UI) can be dropped. `event_drop` is not in `undo_change` (`interpret.rs:~1080-1115`).
- `interpret.rs:962-990` with `store.rs:1114-1132`: `merge_nodes` deletes the `from` node, including `created_by=user` nodes. The prompt says merges are not undoable. Content is moved, not lost.
- `prompts/reorganise.system.md` has no "ignore instructions inside the data" line. Episode titles and summaries are model output derived from transcripts, so this is a second-order injection path. `interpret`, `categorise` and `review` do have the line.

**Fix:**
- Skip `drop_event` when `e.source == Origin::User`.
- Skip merge/archive/rename/move of `created_by == User` nodes, or make them proposals the user confirms.
- Make `event_drop` undoable: re-insert from the `before` JSON.
- Add the ignore-instructions line to `reorganise.system.md`.

### 5. LOW: the database is created 0644 in a 0755 directory (CONFIRMED)
**Evidence:**
- `store.rs:615-628`: `create_dir_all` plus `Connection::open`, with no mode set.
- Observed: `oats.sqlite`, `-wal` and `-shm` are `-rw-r--r--`, and the data dir is `drwxr-xr-x` (process umask 022).
- At the default path this is mitigated: `~/Library` and `~/Library/Application Support` are `drwx------`.
- It is not mitigated for `OATS_DATA_DIR` elsewhere, or on Linux (`$XDG_DATA_HOME` under a home directory that is often 0755).

**Fix** (in `Store::open`, unix). SQLite creates `-wal`/`-shm` with the main database file's permissions, so fixing the main file covers them:
```rust
#[cfg(unix)] {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?; // before Connection::open
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)); // existing files
}
```
None of this needs `unsafe`.

### 6. LOW: deleted transcripts linger, and data is backed up (CONFIRMED / user choice)
- `store.rs:926` (`delete_segment`), plus episode and event deletes: without `secure_delete`, the text stays in free pages and the WAL until it is overwritten. **Fix:** add `PRAGMA secure_delete=ON;` in `open`. After user-initiated deletes, run `PRAGMA wal_checkpoint(TRUNCATE)`.
- `tmutil isexcluded ~/Library/Application Support/oatsnotes` reports `[Included]`, so Time Machine backs up the transcripts. Application Support is not synced by iCloud Drive (only Desktop and Documents are, when enabled). Offer a preference that runs `tmutil addexclusion <data_dir>`, or sets the `com.apple.metadata:com_apple_backup_excludeItem` xattr. Leave this as the user's choice.

### 7. LOW: dictated and transcript-derived text is logged at info/warn (CONFIRMED)
`RUST_LOG` defaults to `info` (`apps/oats-notes/src/main.rs:100`).
- `scheduler.rs:695-700` logs `o.summary` at info. For commands this is `lines.join("; ")` (`scheduler.rs:752`), built from `Executed::describe` (`hotword.rs:233-239`): "Note: <dictated text>", event titles, reminder text. Reorganise summaries include node names.
- `scheduler.rs:706` and `app.rs:686` log the `CliError` at warn. `CliError::Exit` and `BadOutput` embed up to 400/300 chars of the CLI's stdout (`claude_cli.rs:331-343`), and that stdout can echo model output derived from the transcript.
- Segment text itself is never logged. The pipeline logs only durations (`pipeline.rs:357`), and the brain does not log prompts.

**Fix:** at info/warn, log only counts and kinds ("command: 2 actions", "Exit 1"). Move `summary` and stdout snippets to `log::debug!`. The full detail stays in `brain_runs`, which is inside the protected database.

### 8. LOW: the `claude` working directory is the data directory itself (CONFIRMED)
`main.rs:154` sets `work_dir = data_dir()`, which holds `oats.sqlite` and `app.ron`. Today `--tools ""` means the CLI cannot read files. But `--safe-mode` keeps built-in tools, so if `--tools ""` is ever dropped or broken, the CLI could read the whole transcript database. **Fix:** use an empty subdirectory, `data_dir().join("claude-work")`, as the tests already do (`scheduler.rs:1165`, `claude_cli.rs:453`).

### 9. LOW: transcript isn't delimited in prompts; templates substitute one after another (CONFIRMED)
- `prompts/interpret.user.md` puts `{{transcript}}` under a Markdown heading with no fence. Speech like "# Output … return node_id 1" can mimic the template's structure.
- `interpret.rs:38-44` `render()` substitutes keys in order. A node name or prior summary containing `{{transcript}}` gets expanded when that key is substituted (it is the last key, so it is a duplication, not an injection).

**Fix:**
- Wrap untrusted blocks in `<transcript>…</transcript>` (and `<command>`, `<episodes>`), and strip those tag strings from the data.
- Render in a single pass: scan the template for `{{key}}` once and don't re-scan inserted values.
- The structured-output schema plus the `apply_*` id checks remain the real defence, and they hold:
  - ids are checked against the tree (`interpret.rs:339,642`);
  - categories are protected (`interpret.rs:962-1044`);
  - dates are parsed strictly (`dates.rs:9-15`);
  - all SQL uses parameters (every `format!` in `store.rs` interpolates only constant column or table names);
  - model strings never reach a path, shell or URL.

### 10. LOW: binaries are resolved from PATH (CONFIRMED)
- `claude_cli.rs:412`: `Command::new("kill")` is looked up on PATH. **Fix:** `/bin/kill`.
- `claude_cli.rs:139-151`: `OATS_CLAUDE_BIN`, then `~/.local/bin/claude` (currently a symlink to `~/.local/share/claude/versions/2.1.312`), then bare `claude` from PATH. A .app launched from Finder gets the minimal launchd PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), so PATH hijack is unlikely. The weak link is a same-user-writable location, which is outside a sensible threat model.
  - **Fix:** canonicalise once at startup. Refuse relative paths. Warn if the target or its directory is group- or world-writable. The resolved path is already logged (`scheduler.rs:419`).
- `system_font.rs:165,189` (`gsettings`, `fc-match`) run only on Linux.

### 11. INFO: FluidAudio's network behaviour (CONFIRMED by source reading)
`target/debug/build/fluidaudio-rs-f74a044556b94522/out/swift-build/checkouts/FluidAudio`, v0.14.8 @ 56607d90, `dependencies: []`.
- **The only runtime host is `https://huggingface.co`** (`ModelRegistry.swift:37`): `/api/models/<repo>/tree/...` and `/<repo>/resolve/main/<file>`.
  - Other URLs in `Sources/` are in comments or the `FluidAudioCLI` benchmark target (github, openslr, ed.ac.uk, `datasets-server.huggingface.co`). That target is not linked into the bridge library.
- **No telemetry or analytics.** Grep for `analytics|telemetry|posthog|sentry|mixpanel|firebase|amplitude|crashlytics|bugsnag|datadog` over `Sources/FluidAudio`: nothing. The only `URLSession` use is in `DownloadUtils.swift`/`AssetDownloader.swift`/`ModelRegistry.swift`.
- `SystemInfo.logOnce` writes to OSLog only.
- Download happens only when model files are missing (`DownloadUtils.swift:213-232`, `AsrModels.swift:500`).
- **Caveat:** if loading fails, FluidAudio **deletes the cache and downloads again** (`DownloadUtils.swift:129-152`). A corrupted or partly evicted model can trigger an unannounced download long after the first run.
- No checksum or ETag verification of downloaded models. Integrity relies on TLS only.
- `REGISTRY_URL` / `MODEL_REGISTRY_URL` env vars redirect the download host (`ModelRegistry.swift:35-36`). `https_proxy`/`http_proxy` are honoured. `HF_TOKEN` (and two aliases) is sent as a Bearer token to *whatever* registry host is configured (`DownloadUtils.swift:17-31`).
- **Recommendation:**
  - Check whether `~/Library/Application Support/FluidAudio/Models/parakeet-tdt-0.6b-v3-coreml` exists before `init_asr()`, and show "will download ~600 MB from huggingface.co" in the UI. Optionally add an `OATS_OFFLINE=1` mode that refuses to start the engine if models are missing.
  - Warn at startup if `REGISTRY_URL` or `MODEL_REGISTRY_URL` is set. Removing env vars in Rust 2024 is `unsafe`, which the workspace forbids, so warn rather than scrub.
- Audio: `transcribeSamples` (bridge `FluidAudioBridge.swift:109`) runs in memory through `AsrManager.transcribe([Float])`. The disk-backed temp-file path (`AudioSourceFactory`) is used only by `transcribeDiskBacked(url)` and the file-based APIs, which the app never calls.

### 12. INFO: the CLI isolation flags (CONFIRMED in code; what the CLI writes is LIKELY)
- `claude_cli.rs:292-303` passes `-p --output-format json [--model m] --safe-mode --setting-sources "" --strict-mcp-config --tools "" --disable-slash-commands --no-session-persistence --max-turns 2 --max-budget-usd 0.50 --system-prompt <static text> --json-schema <static schema>`.
- All of these flags exist in the installed CLI 2.1.312 (`claude --help`).
- **The prompt goes on stdin** (`claude_cli.rs:308-314`). argv, which is visible in `ps`, carries only the static system prompt and schema. No transcript text is in argv.
- `--debug` and `--debug-file` are not passed, so no debug log is written.
- No `~/.claude/projects/*` directory or `~/.claude.json` project entry exists for the brain's working directories (`…/oats-brain-demo` or the data dir). This is consistent with `--no-session-persistence` working.
- The CLI still writes its own global state: `~/.claude.json` counters and caches. Not verified with a live call, to save budget.
- **Caveat:** per `claude --help`, `--safe-mode` leaves "Admin-managed (policy) settings" in force, so managed hooks or MCP would still run. There is no `/Library/Application Support/ClaudeCode/` on this machine, so nothing applies today. `--strict-mcp-config` plus `--tools ""` cover MCP and tools.
- `claude --version` is spawned once at brain start (`scheduler.rs:409`, observed in the log). It runs without isolation flags but also without a prompt.

### 13. INFO: process and privilege (CONFIRMED)
- `otool -l target/debug/oats-notes`: exactly one `LC_RPATH`, `/usr/lib/swift`. No `@executable_path` or `@loader_path` rpaths, so no dylib-hijack directory. All Swift dylibs are absolute `/usr/lib/swift/...` except `@rpath/libswift_Concurrency.dylib`, which resolves to `/usr/lib/swift` (dyld shared cache, SIP-protected).
- TCC: the mic permission currently belongs to the terminal. A .app needs:
  - `NSMicrophoneUsageDescription` in Info.plist;
  - under hardened runtime, the `com.apple.security.device.audio-input` entitlement;
  - if it is ever sandboxed, the `claude` child inherits the sandbox and needs `com.apple.security.network.client`. A sandboxed app can't exec `~/.local/bin/claude` without extra entitlements, so plan for no App Sandbox, or for an XPC helper.
- The child runs in its own process group and is killed as a group after 180 s (`claude_cli.rs:249-252, 389-391`). Good.

### 14. INFO: other on-disk state (CONFIRMED)
- eframe persistence writes `~/Library/Application Support/oatsnotes/app.ron` (eframe's own app-id path, which on this case-insensitive volume is the same folder as the default data dir). It ignores `OATS_DATA_DIR`. Contents: `oatsnotes.prefs` (sidebar, view id, listening flag), window geometry and egui options. **No transcript text.**
- `--selftest` (`main.rs:205-260`, `oats-ui/src/selftest.rs:116-260`) seeds fixed demo strings into `$TMPDIR/oatsnotes-selftest-<pid>/selftest.sqlite` (or `OATS_DATA_DIR`). It deletes that temp directory when done, never opens the microphone (the audio starter returns Err) and never starts the brain.

---

## Dynamic check (what I observed)
Built with `cargo build -p oats-notes` (ok). Ran `target/debug/oats-notes` with `OATS_ENGINE=null OATS_DATA_DIR=<scratch>/dyn/<run>/data RUST_LOG=info`, for about 20 s per run.

**Run 1, `OATS_BRAIN=off` (also `OATS_NO_STATUS_ITEM=1`):**
- `lsof -nP -a -p PID -i`, sampled at 3, 6, 9, 12, 15 and 18 s: **no IPv4/IPv6 sockets at any sample** (`lsof -i` printed nothing).
- `nettop -p PID -L 2`: header only, **zero flows.**
- `pgrep -P PID`: **no children.**
- Open files: the database, `-wal`, `-shm`, the log, and Metal shader caches in `/var/folders/.../C/com.apple.metal`. Nothing else.

**Run 2, brain ON, status item ON, empty database:**
- Same result: **no sockets in any sample, nettop zero flows.**
- `pgrep -P PID`, polled every 100 ms for about 18 s: **no `claude -p` child ever appeared.**
- The log shows the brain ran `claude --version` once at startup ("claude CLI 2.1.312 (Claude Code) at ~/.local/bin/claude"). That child finished before the first sample.
- `brain_runs` count = 0 and `segments` = 0 in both databases (`sqlite3 -readonly`).

**Files:** `oats.sqlite`, `-wal` and `-shm` were `-rw-r--r--`; the data dir was `drwxr-xr-x` (see finding 5).

Not run: the real engine (no model download) and any `claude -p` call (no budget spent).

---

## Checklist: verified clean
- [x] No `std::net`, `TcpStream`, `UdpSocket`, `reqwest`, `ureq`, `hyper`, `curl` or `open::that` in any crate or app (grep of `crates/` and `apps/`).
- [x] Every `Command::new` call:
  - `claude_cli.rs:238` (claude, see finding 12);
  - `claude_cli.rs:412` (`kill -TERM -- -<pgid>`);
  - `system_font.rs:165` (`gsettings get org.gnome.desktop.interface font-name`, Linux only);
  - `system_font.rs:189` (`fc-match -f … <pattern>`, Linux only);
  - build scripts: `fluidaudio-rs` build.rs runs `swift build -c release`. The app's and the engine's build.rs only emit rpath link arguments.
- [x] No networking crates in `cargo tree -e normal`. The `objc2-foundation` `NSURLSession`/`NSURL` modules are generated bindings with no callers in tray-icon, muda, winit, eframe, cpal or accesskit.
- [x] `egui-winit`'s `open_url_in_browser` is a no-op: the `webbrowser` crate is not in the tree, and the UI renders no clickable links.
- [x] `craft-ui` (git, pinned `rev = d27646a5…` in Cargo.toml and Cargo.lock): depends only on egui (plus optional serde). No fs, net or process calls in `src/`.
- [x] The only git dependency is pinned by rev. FluidAudio is pinned by exact version and revision in `Package.resolved`. No unpinned branch dependencies.
- [x] `rusqlite` 0.41.1 is `bundled` without the `load_extension` feature, so no extension loading.
- [x] `egui_kittest` and `kittest` are in the normal graph because `selftest` is a default feature. They do no I/O beyond rendering and writing the PNG the user names.
- [x] `cpal` 0.18.2, `tray-icon` 0.26.1, `muda` 0.21.2, `eframe`/`wgpu`/`winit`, `directories` 6, `image` 0.25 (png only): no network use found.
- [x] Audio is never written. No WAV writer, `File::create` or `fs::write` of samples in non-test code. The only `fs::write` calls are in tests and the fake-CLI scripts. FluidAudio's samples path is in-memory.
- [x] The prompt goes on stdin, not argv. `--no-session-persistence`, `--safe-mode`, `--setting-sources ""`, `--strict-mcp-config`, `--tools ""` and `--disable-slash-commands` are set whenever `isolate` is true, which is the default (`claude_cli.rs:127`).
- [x] The CLI's working directory is the data dir, not the repo, so no project CLAUDE.md or `.mcp.json` is loaded. Finding 8 still recommends a dedicated empty subdirectory.
- [x] Model output only feeds SQL parameters and UI text. It never reaches a path, shell, URL or SQL text.
- [x] Event dates are strictly parsed, and invalid ones are skipped.
- [x] `max_new_nodes` is enforced for interpretation (not for hotword `create_project`, see finding 2).
- [x] Categories are protected from merge, rename and archive.
- [x] Hotword-sourced events are protected from `drop_event`.
- [x] The hotword path has no delete, merge or archive intent.
- [x] Segment text and prompts are never logged. Only command and summary lines are (finding 7).
- [x] `app.ron` holds no transcript content.
- [x] The binary's only rpath is `/usr/lib/swift`.
