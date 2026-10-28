//! OatsNotes' brain — the intelligence that turns the all-day transcript into notes, filing,
//! a calendar and spoken commands, through the locally installed Claude Code CLI (`claude`,
//! the user's own login; no API key). Platform-independent; depends on `oats-core` only.
//!
//! - [`claude_cli`]: the ~300-line wrapper that runs `claude -p --output-format json
//!   --json-schema …` with the prompt on stdin and parses the single result object.
//! - [`episodes`]: cuts the day's segments into episodes (silence gap, length cap) — pure.
//! - [`interpret`]: the prompts (`prompts/*.md`, editable) and the jobs: episode
//!   interpretation (title, summary, filing into the node tree, decisions, action items,
//!   topics, significant dates), per-segment categorisation, the coherence review and the
//!   tree reorganisation — build request → parse → apply to the store, all testable with
//!   canned JSON.
//! - [`hotword`]: "hey computer, …" detected on transcript text, turned into actions (event,
//!   reminder, note, project) by a dedicated prompt and executed against the store.
//! - [`dates`]: local-time date helpers shared by the above.
//! - [`scheduler`]: the worker thread with the tiered job model (reactive haiku jobs; the
//!   15-minute rolling-window interpretation; the 4-hourly review; the nightly
//!   reorganisation), its budget guard and its event stream to the UI.
//!
//! See `docs/research-claude-cli-and-interpretation.md` for where the design comes from.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod claude_cli;
pub mod dates;
pub mod episodes;
pub mod hotword;
pub mod interpret;
pub mod scheduler;

pub use claude_cli::{ClaudeCli, CliError, Request, Response, Version};
pub use episodes::{Chunk, EpisodeRule};
pub use hotword::{Executed, HotwordConfig};
pub use scheduler::{Brain, BrainCommand, BrainConfig, BrainEvent, BrainHandle, Tier, TierModels};
