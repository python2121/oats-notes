//! Voiceprints: matching a speaker embedding (from [`crate::SpeechEngine::speaker_spans`]) to a
//! known voice, or opening a new one. Pure — no I/O; the pipeline loads the book from the store
//! at session start ([`crate::Store::speakers`]) and writes the changed entries back in batches.
//!
//! ONE global book (a person is the same person on the microphone and on a call). Each voice
//! has a centroid (an L2-normalised running mean of its embeddings, weighted by speech seconds)
//! and matching is cosine distance (`1 − a·b` of unit vectors) to every centroid — a plain loop
//! over a few hundred 256-float slices, no allocation per comparison:
//! - distance ≤ [`ASSIGN_THRESHOLD`] → that voice (the nearest);
//! - else a NEW voice when the span has ≥ [`MIN_NEW_SPEAKER_SECS`] of speech (else nothing:
//!   too short to trust);
//! - the centroid moves toward the new embedding only when it is close (≤
//!   [`UPDATE_THRESHOLD`]) and long enough (≥ [`MIN_UPDATE_SECS`]): weighted mean with the
//!   voice's weight capped at [`MAX_WEIGHT_SECS`], so a long history still adapts.
//!
//! The thresholds: assign 0.7 (FluidAudio's `DiarizerConfig.clusteringThreshold`; its
//! `SpeakerManager` uses 0.65 — a laptop microphone in a room is noisier than its benchmarks),
//! update 0.45 (its default), a NEW voice only from ≥ 3 s of speech (a 2 s room-mic span of the
//! same person came out ~0.65 from a centroid built on longer ones; 3 s clips sit within 0.2),
//! update from ≥ 2 s (FluidAudio: 1 s /
//! 2 s), whose WeSpeaker embeddings we use.

/// Cosine distance at or under which an embedding belongs to a known voice.
pub const ASSIGN_THRESHOLD: f32 = 0.7;
/// Cosine distance at or under which a voice's centroid learns from the embedding.
pub const UPDATE_THRESHOLD: f32 = 0.45;
/// A span shorter than this never opens a new voice.
pub const MIN_NEW_SPEAKER_SECS: f64 = 3.0;
/// A span shorter than this never moves a centroid.
pub const MIN_UPDATE_SECS: f64 = 2.0;
/// The weight of a voice's history in the running mean is capped here (seconds of speech).
pub const MAX_WEIGHT_SECS: f64 = 600.0;
/// With no voice flagged as the user's, the first voice with this much MICROPHONE speech in
/// total becomes "me".
pub const AUTO_ME_MIC_SECS: f64 = 60.0;
/// Two voices whose centroids come within this distance are the same person: merged
/// (different voices sit at 0.8+; the same voice across clip lengths within 0.2).
pub const MERGE_THRESHOLD: f32 = 0.3;
/// …only when both have at least this much speech (a young centroid is still noisy).
pub const MIN_MERGE_SECS: f64 = 30.0;

/// `OATS_SPEAKERS=on|off` (also 1/0, true/false): `None` when unset or unreadable (logged).
pub fn speakers_env() -> Option<bool> {
    let v = std::env::var("OATS_SPEAKERS").ok()?;
    match v.trim().to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Some(true),
        "off" | "0" | "false" | "no" => Some(false),
        _ => {
            log::warn!("OATS_SPEAKERS={v:?} is not on|off; ignored");
            None
        }
    }
}

/// Cosine distance of two vectors (`1 − cos`; 0 = same direction, 2 = opposite). Vectors of
/// different length or a zero vector are as far as can be (2).
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 2.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return 2.0;
    }
    (1.0 - dot / (na.sqrt() * nb.sqrt())).clamp(0.0, 2.0)
}

/// Scale `v` to unit length in place (a zero vector stays zero). `false` when it was zero or
/// not finite.
pub fn normalise(v: &mut [f32]) -> bool {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !n.is_finite() || n <= f32::EPSILON {
        return false;
    }
    for x in v.iter_mut() {
        *x /= n;
    }
    true
}

/// Every stored voice by id (the UI's labels, the brain's transcript).
pub type SpeakerMap = std::collections::HashMap<i64, Speaker>;

/// Who named a voice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameSource {
    /// The user (Preferences, a transcript row's "Speaker ▸ Name…"): never overwritten by
    /// the brain.
    User,
    /// The brain, from conversational clues (direct address and reply, a self-introduction).
    Brain,
}

impl NameSource {
    pub fn as_str(self) -> &'static str {
        match self {
            NameSource::User => "user",
            NameSource::Brain => "brain",
        }
    }

    pub fn parse(s: &str) -> Option<NameSource> {
        match s {
            "user" => Some(NameSource::User),
            "brain" => Some(NameSource::Brain),
            _ => None,
        }
    }
}

/// A stored voice (`speakers` row, schema v10).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Speaker {
    pub id: i64,
    pub name: Option<String>,
    pub name_source: Option<NameSource>,
    /// The brain's confidence in its name (a later brain name replaces it only when higher).
    pub name_confidence: Option<f32>,
    pub is_me: bool,
    /// Unit length, 256 floats for FluidAudio.
    pub centroid: Vec<f32>,
    pub speech_secs: f64,
    pub mic_secs: f64,
    pub created_at: i64,
    pub last_heard_at: i64,
}

impl Speaker {
    /// The book's view of it.
    pub fn entry(&self) -> SpeakerEntry {
        SpeakerEntry {
            id: self.id,
            centroid: self.centroid.clone(),
            speech_secs: self.speech_secs,
            mic_secs: self.mic_secs,
            is_me: self.is_me,
            user_named: self.name.is_some() && self.name_source == Some(NameSource::User),
            last_heard_at: self.last_heard_at,
            dirty: false,
        }
    }

    /// A brain name: `name_source = brain`.
    pub fn brain_named(&self) -> bool {
        self.name.is_some() && self.name_source == Some(NameSource::Brain)
    }
}

/// How a transcript line is labelled, everywhere (transcript rows, the brain's transcript):
/// the user's voice (`is_me`) is "me"; a named voice its name; an anonymous voice "them N" on
/// system audio or "voice N" on the microphone (N = the voice's id, stable across sessions);
/// a line with no (known) voice keeps its lane's label: "me" (microphone) / "them" (system).
pub fn speaker_label(speaker: Option<&Speaker>, source: crate::engine::AudioSourceKind) -> String {
    use crate::engine::AudioSourceKind;
    match speaker {
        Some(s) if s.is_me => "me".to_owned(),
        Some(s) => match s.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) => name.to_owned(),
            None => match source {
                AudioSourceKind::System => format!("them {}", s.id),
                AudioSourceKind::Mic => format!("voice {}", s.id),
            },
        },
        None => match source {
            AudioSourceKind::System => "them".to_owned(),
            AudioSourceKind::Mic => "me".to_owned(),
        },
    }
}

/// One known voice as the book holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerEntry {
    pub id: i64,
    /// Unit length.
    pub centroid: Vec<f32>,
    /// Total speech assigned to this voice, seconds (the running mean caps its weight at
    /// [`MAX_WEIGHT_SECS`]).
    pub speech_secs: f64,
    /// …of which heard on the microphone (the auto-"me" rule).
    pub mic_secs: f64,
    pub is_me: bool,
    /// Named by the user: two such voices are never merged automatically.
    pub user_named: bool,
    pub last_heard_at: i64,
    /// Changed since the last [`SpeakerBook::take_dirty`].
    pub dirty: bool,
}

impl SpeakerEntry {
    pub fn new(id: i64, mut centroid: Vec<f32>, speech_secs: f64, last_heard_at: i64) -> Self {
        normalise(&mut centroid);
        Self {
            id,
            centroid,
            speech_secs,
            mic_secs: 0.0,
            is_me: false,
            user_named: false,
            last_heard_at,
            dirty: false,
        }
    }
}

/// Two voices the book found to be one person ([`SpeakerBook::merge_if_close`]): `from` was
/// folded into `into` (the caller merges the store rows the same way).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Merge {
    pub from: i64,
    pub into: i64,
    pub distance: f32,
}

/// What [`SpeakerBook::assign`] decided for one span.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Assignment {
    pub speaker_id: i64,
    /// A new voice was opened for it.
    pub created: bool,
    /// Distance to the chosen voice's centroid (before any update; 0 for a new voice).
    pub distance: f32,
    /// The centroid learned from it.
    pub updated: bool,
}

/// The known voices. See the module docs.
#[derive(Clone, Debug, Default)]
pub struct SpeakerBook {
    entries: Vec<SpeakerEntry>,
}

impl SpeakerBook {
    pub fn new(entries: Vec<SpeakerEntry>) -> Self {
        Self { entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[SpeakerEntry] {
        &self.entries
    }

    pub fn get(&self, id: i64) -> Option<&SpeakerEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// The nearest voice and its distance (`None` for an empty book).
    pub fn nearest(&self, embedding: &[f32]) -> Option<(usize, f32)> {
        let mut best: Option<(usize, f32)> = None;
        for (i, e) in self.entries.iter().enumerate() {
            let d = cosine_distance(embedding, &e.centroid);
            if best.is_none_or(|(_, b)| d < b) {
                best = Some((i, d));
            }
        }
        best
    }

    /// Assign one span (`embedding`, `speech_secs` of speech, heard at `now_ms`, on the
    /// microphone when `mic`). A new voice gets its id from `create(centroid, speech_secs)`
    /// (the store's insert; `None` = could not be created → no assignment). `None` also when
    /// the span is too short to open a voice, or the embedding is unusable.
    pub fn assign(
        &mut self,
        embedding: &[f32],
        speech_secs: f64,
        mic: bool,
        now_ms: i64,
        create: impl FnOnce(&[f32], f64) -> Option<i64>,
    ) -> Option<Assignment> {
        if embedding.is_empty() || !embedding.iter().all(|x| x.is_finite()) {
            return None;
        }
        let speech_secs = speech_secs.max(0.0);
        if let Some((i, distance)) = self.nearest(embedding)
            && distance <= ASSIGN_THRESHOLD
        {
            let e = &mut self.entries[i];
            let updated = distance <= UPDATE_THRESHOLD && speech_secs >= MIN_UPDATE_SECS;
            if updated {
                let mut new = embedding.to_vec();
                normalise(&mut new);
                let w_old = e.speech_secs.min(MAX_WEIGHT_SECS) as f32;
                let w_new = speech_secs as f32;
                let total = w_old + w_new;
                if total > 0.0 {
                    for (c, x) in e.centroid.iter_mut().zip(&new) {
                        *c = (*c * w_old + x * w_new) / total;
                    }
                    normalise(&mut e.centroid);
                }
            }
            e.speech_secs += speech_secs;
            if mic {
                e.mic_secs += speech_secs;
            }
            e.last_heard_at = e.last_heard_at.max(now_ms);
            e.dirty = true;
            return Some(Assignment {
                speaker_id: e.id,
                created: false,
                distance,
                updated,
            });
        }
        if speech_secs < MIN_NEW_SPEAKER_SECS {
            return None;
        }
        let mut centroid = embedding.to_vec();
        if !normalise(&mut centroid) {
            return None;
        }
        let id = create(&centroid, speech_secs)?;
        let mut entry = SpeakerEntry::new(id, centroid, speech_secs, now_ms);
        if mic {
            entry.mic_secs = speech_secs;
        }
        // the row was just written with these values; only the mic seconds are pending
        entry.dirty = mic;
        self.entries.push(entry);
        Some(Assignment {
            speaker_id: id,
            created: true,
            distance: 0.0,
            updated: false,
        })
    }

    /// The voice to flag as the user's, when none is yet: the first with ≥
    /// [`AUTO_ME_MIC_SECS`] of microphone speech (most microphone speech first). The caller
    /// stores it and calls [`Self::set_me`].
    pub fn auto_me(&self) -> Option<i64> {
        if self.entries.iter().any(|e| e.is_me) {
            return None;
        }
        self.entries
            .iter()
            .filter(|e| e.mic_secs >= AUTO_ME_MIC_SECS)
            .max_by(|a, b| a.mic_secs.total_cmp(&b.mic_secs))
            .map(|e| e.id)
    }

    /// Flag `id` as the user's voice (and no other).
    pub fn set_me(&mut self, id: i64) {
        for e in &mut self.entries {
            e.is_me = e.id == id;
        }
    }

    /// After `id`'s centroid moved: when it now sits within [`MERGE_THRESHOLD`] of another
    /// voice and both carry ≥ [`MIN_MERGE_SECS`] of speech, they are one person — folded
    /// into one entry (the user's voice survives, else a user-named one, else the older;
    /// two user-named voices are never merged: that is the user's call) with the same
    /// weighted centroid the store computes. The caller merges the rows with
    /// `Store::merge_speakers(from, into)`.
    pub fn merge_if_close(&mut self, id: i64) -> Option<Merge> {
        let i = self.entries.iter().position(|e| e.id == id)?;
        if self.entries[i].speech_secs < MIN_MERGE_SECS {
            return None;
        }
        let (j, distance) = self
            .entries
            .iter()
            .enumerate()
            .filter(|(j, e)| *j != i && e.speech_secs >= MIN_MERGE_SECS)
            .map(|(j, e)| (j, cosine_distance(&self.entries[i].centroid, &e.centroid)))
            .min_by(|a, b| a.1.total_cmp(&b.1))?;
        if distance > MERGE_THRESHOLD {
            return None;
        }
        let (a, b) = (&self.entries[i], &self.entries[j]);
        if a.user_named && b.user_named {
            return None;
        }
        let rank = |e: &SpeakerEntry| (e.is_me, e.user_named, std::cmp::Reverse(e.id));
        let (keep, drop) = if rank(a) >= rank(b) { (i, j) } else { (j, i) };
        let gone = self.entries.remove(drop);
        let keep = if drop < keep { keep - 1 } else { keep };
        let e = &mut self.entries[keep];
        let (wa, wb) = (
            gone.speech_secs.min(MAX_WEIGHT_SECS) as f32,
            e.speech_secs.min(MAX_WEIGHT_SECS) as f32,
        );
        if wa + wb > 0.0 {
            for (c, x) in e.centroid.iter_mut().zip(&gone.centroid) {
                *c = (*c * wb + x * wa) / (wa + wb);
            }
            normalise(&mut e.centroid);
        }
        e.speech_secs += gone.speech_secs;
        e.mic_secs += gone.mic_secs;
        e.is_me |= gone.is_me;
        e.user_named |= gone.user_named;
        e.last_heard_at = e.last_heard_at.max(gone.last_heard_at);
        e.dirty = true;
        Some(Merge {
            from: gone.id,
            into: e.id,
            distance,
        })
    }

    /// The entries changed since the last call (their dirty flags cleared).
    pub fn take_dirty(&mut self) -> Vec<SpeakerEntry> {
        let mut out = Vec::new();
        for e in &mut self.entries {
            if e.dirty {
                e.dirty = false;
                out.push(e.clone());
            }
        }
        out
    }

    pub fn has_dirty(&self) -> bool {
        self.entries.iter().any(|e| e.dirty)
    }
}

/// The speaker who speaks most in a segment, from its assigned (speaker id, speech seconds)
/// spans, summing the seconds per speaker. `None` when nothing was assigned.
pub fn dominant(assigned: &[(i64, f64)]) -> Option<i64> {
    let mut best: Option<(i64, f64)> = None;
    for &(id, _) in assigned {
        let total: f64 = assigned
            .iter()
            .filter(|(i, _)| *i == id)
            .map(|(_, s)| s)
            .sum();
        if best.is_none_or(|(_, b)| total > b) {
            best = Some((id, total));
        }
    }
    best.map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIM: usize = 256;

    /// A deterministic pseudo-random unit vector.
    fn base(seed: u64) -> Vec<f32> {
        let mut x = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut v: Vec<f32> = (0..DIM)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((x >> 33) as f32 / (1u64 << 31) as f32) - 0.5
            })
            .collect();
        normalise(&mut v);
        v
    }

    /// `center` plus `amount` of noise (seeded), normalised.
    fn near(center: &[f32], seed: u64, amount: f32) -> Vec<f32> {
        let noise = base(seed + 1000);
        let mut v: Vec<f32> = center
            .iter()
            .zip(&noise)
            .map(|(c, n)| c + amount * n)
            .collect();
        normalise(&mut v);
        v
    }

    fn counter() -> impl FnMut(&[f32], f64) -> Option<i64> {
        let mut next = 0;
        move |_, _| {
            next += 1;
            Some(next)
        }
    }

    #[test]
    fn distances() {
        let a = base(1);
        assert!(cosine_distance(&a, &a) < 1e-5);
        let neg: Vec<f32> = a.iter().map(|x| -x).collect();
        assert!((cosine_distance(&a, &neg) - 2.0).abs() < 1e-4);
        assert_eq!(cosine_distance(&a, &a[..10]), 2.0);
        assert_eq!(cosine_distance(&a, &vec![0.0; DIM]), 2.0);
        // random directions in 256-d are nearly orthogonal: far apart
        assert!(cosine_distance(&base(1), &base(2)) > 0.8);
    }

    #[test]
    fn two_clusters_assign_consistently() {
        let (alice, bob) = (base(1), base(2));
        let mut book = SpeakerBook::default();
        let mut create = counter();
        let mut ids = (Vec::new(), Vec::new());
        for i in 0..10 {
            let a = book.assign(&near(&alice, i, 0.3), 3.0, true, i as i64, &mut create);
            let b = book.assign(&near(&bob, 50 + i, 0.3), 3.0, false, i as i64, &mut create);
            ids.0.push(a.map(|a| a.speaker_id));
            ids.1.push(b.map(|b| b.speaker_id));
        }
        assert!(ids.0.iter().all(|i| *i == Some(1)), "{ids:?}");
        assert!(ids.1.iter().all(|i| *i == Some(2)), "{ids:?}");
        assert_eq!(book.len(), 2);
        let alice_entry = book.get(1).map(|e| (e.speech_secs, e.mic_secs));
        assert_eq!(alice_entry, Some((30.0, 30.0)));
        assert_eq!(book.get(2).map(|e| e.mic_secs), Some(0.0));
    }

    #[test]
    fn voices_that_drift_close_merge_into_one_unless_both_are_user_named() {
        let mut book = SpeakerBook::default();
        let mut create = counter();
        book.assign(&base(1), 40.0, true, 1, &mut create);
        book.assign(&base(2), 40.0, false, 2, &mut create);
        assert_eq!(book.len(), 2, "unrelated at creation: two voices");
        // voice 2's centroid moves next to voice 1's
        let id2 = book.entries[1].id;
        book.entries[1].centroid = near(&base(1), 8, 0.05);
        normalise(&mut book.entries[1].centroid);
        let none = Merge {
            from: 0,
            into: 0,
            distance: 9.0,
        };
        let m = book.merge_if_close(id2).unwrap_or(none);
        assert_eq!((m.from, m.into), (id2, 1), "the older voice survives");
        assert!(m.distance < MERGE_THRESHOLD);
        assert_eq!(book.len(), 1);
        assert_eq!(book.get(1).map(|e| e.speech_secs), Some(80.0));
        assert_eq!(book.get(1).map(|e| e.mic_secs), Some(40.0));
        // young voices (under MIN_MERGE_SECS) are left alone
        book.assign(&base(4), 5.0, false, 3, &mut create);
        assert_eq!(book.len(), 2);
        let young = book.entries[1].id;
        assert_eq!(book.merge_if_close(young), None);
        // two user-named voices are never merged automatically; "me" wins the survivor rank
        let mut book = SpeakerBook::default();
        book.assign(&base(2), 40.0, false, 1, &mut create);
        book.assign(&base(3), 40.0, false, 2, &mut create);
        book.entries[1].centroid = near(&base(2), 4, 0.05);
        normalise(&mut book.entries[1].centroid);
        let (a, b) = (book.entries[0].id, book.entries[1].id);
        book.entries[0].user_named = true;
        book.entries[1].user_named = true;
        assert_eq!(book.merge_if_close(b), None);
        book.entries[0].user_named = false;
        book.set_me(b);
        let m = book.merge_if_close(b).unwrap_or(none);
        assert_eq!((m.from, m.into), (a, b));
        assert!(book.get(b).is_some_and(|e| e.is_me && e.user_named));
    }

    #[test]
    fn a_far_embedding_opens_a_new_voice_and_a_short_one_does_not() {
        let mut book = SpeakerBook::default();
        let mut create = counter();
        let first = book.assign(&base(1), 3.0, false, 0, &mut create);
        assert_eq!(first.map(|a| (a.speaker_id, a.created)), Some((1, true)));
        assert_eq!(book.assign(&base(2), 0.5, false, 0, &mut create), None);
        assert_eq!(book.assign(&base(2), 2.5, false, 0, &mut create), None);
        let second = book.assign(&base(2), 3.0, false, 0, &mut create);
        assert_eq!(second.map(|a| (a.speaker_id, a.created)), Some((2, true)));
        // a short span of a KNOWN voice is still assigned (no new voice needed)
        let short = book.assign(&near(&base(1), 3, 0.1), 0.3, false, 0, &mut create);
        assert_eq!(
            short.map(|a| (a.speaker_id, a.created, a.updated)),
            Some((1, false, false))
        );
        // a failed insert assigns nothing
        assert_eq!(book.assign(&base(9), 3.0, false, 0, |_, _| None), None);
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn the_centroid_moves_toward_new_evidence() {
        let start = base(1);
        let mut book = SpeakerBook::new(vec![SpeakerEntry::new(7, start.clone(), 10.0, 0)]);
        let target = near(&start, 5, 0.5);
        let d0 = cosine_distance(&start, &target);
        assert!(d0 <= UPDATE_THRESHOLD, "{d0}");
        let a = book.assign(&target, 10.0, false, 5, |_, _| None);
        assert_eq!(a.map(|a| a.updated), Some(true));
        let c = book.get(7).map(|e| e.centroid.clone()).unwrap_or_default();
        let d1 = cosine_distance(&c, &target);
        assert!(d1 < d0 * 0.75, "{d0} → {d1}");
        assert!((c.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-4);
        // too short to learn from: assigned, centroid unchanged
        let before = book.get(7).map(|e| e.centroid.clone());
        let a = book.assign(&target, 1.0, false, 6, |_, _| None);
        assert_eq!(a.map(|a| a.updated), Some(false));
        assert_eq!(book.get(7).map(|e| e.centroid.clone()), before);
        assert_eq!(book.get(7).map(|e| e.last_heard_at), Some(6));
    }

    #[test]
    fn the_weight_cap_keeps_an_old_voice_adapting() {
        let start = base(1);
        let target = near(&start, 5, 0.5);
        let step = |history: f64| {
            let mut book = SpeakerBook::new(vec![SpeakerEntry::new(1, start.clone(), history, 0)]);
            book.assign(&target, 30.0, false, 0, |_, _| None);
            let c = book.get(1).map(|e| e.centroid.clone()).unwrap_or_default();
            cosine_distance(&start, &c)
        };
        // 10 h of history moves exactly like the cap (600 s), not 60× less
        let capped = step(MAX_WEIGHT_SECS);
        let huge = step(36_000.0);
        assert!(capped > 0.0);
        assert!((capped - huge).abs() < 1e-6, "{capped} vs {huge}");
        assert!(step(60.0) > capped);
    }

    #[test]
    fn auto_me_takes_the_first_voice_with_a_minute_of_mic_speech() {
        let mut book = SpeakerBook::default();
        let mut create = counter();
        let me = base(1);
        let them = base(2);
        for i in 0..5 {
            book.assign(&near(&them, i, 0.2), 20.0, false, 0, &mut create);
            book.assign(&near(&me, 10 + i, 0.2), 10.0, true, 0, &mut create);
        }
        assert_eq!(book.auto_me(), None); // 50 s of mic speech
        book.assign(&near(&me, 99, 0.2), 10.0, true, 0, &mut create);
        let me_id = book.auto_me();
        assert_eq!(me_id, Some(2));
        book.set_me(2);
        assert_eq!(book.auto_me(), None);
        assert!(book.get(2).is_some_and(|e| e.is_me));
    }

    #[test]
    fn dirty_entries_are_taken_once() {
        let mut book = SpeakerBook::new(vec![SpeakerEntry::new(1, base(1), 5.0, 0)]);
        assert!(!book.has_dirty());
        book.assign(&near(&base(1), 2, 0.1), 3.0, false, 9, |_, _| None);
        let d = book.take_dirty();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].speech_secs, 8.0);
        assert!(book.take_dirty().is_empty());
    }

    #[test]
    fn labels() {
        use crate::engine::AudioSourceKind::{Mic, System};
        let mut s = Speaker {
            id: 2,
            ..Speaker::default()
        };
        assert_eq!(speaker_label(None, Mic), "me");
        assert_eq!(speaker_label(None, System), "them");
        assert_eq!(speaker_label(Some(&s), System), "them 2");
        assert_eq!(speaker_label(Some(&s), Mic), "voice 2");
        s.name = Some("Priya".into());
        assert_eq!(speaker_label(Some(&s), System), "Priya");
        s.is_me = true;
        assert_eq!(speaker_label(Some(&s), System), "me");
    }

    #[test]
    fn the_dominant_speaker_sums_its_spans() {
        assert_eq!(dominant(&[]), None);
        assert_eq!(dominant(&[(1, 3.0), (2, 2.0), (2, 2.0)]), Some(2));
        assert_eq!(dominant(&[(1, 3.0), (2, 2.0)]), Some(1));
    }
}
