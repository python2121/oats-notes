# OatsNotes

An always-on, on-device transcription app: it listens all day, transcribes speech locally
(Parakeet TDT on the Apple Neural Engine), and keeps the transcript in a local SQLite database
you can review in the app. No audio or text leaves the machine.

Rust + egui. The speech engine is the only macOS-specific crate; everything else is portable so
a Linux build means swapping one crate.

```bash
cargo run -p oats-notes            # first run downloads the speech model (~600 MB)
OATS_ENGINE=null cargo run -p oats-notes   # no model: UI + speech detection only
```

See `CLAUDE.md` for the architecture, commands and conventions.
