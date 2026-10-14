//! Per-person speaker identification (speaker_id_plan.md Phase A).
//!
//! The orchestrator attributes each turn to a person so memory writes and recall
//! are per-speaker and the LLM can answer with "who am I talking to" context.
//! Approach: **passive + auto-cluster** — an utterance's PCM is embedded into a
//! voiceprint ([`embed`]), matched against enrolled profiles ([`registry`]), and
//! an unrecognized voice mints a new anonymous cluster on the spot. Naming turns a
//! cluster into a person (Phase C).
//!
//! [`SpeakerService`] is the one façade the pipeline holds (behind an `Arc`): give
//! it an utterance, get back a [`SpeakerContext`]. The model runs locally (no raw
//! voice leaves the LAN); Phases A–D exercise it with the deterministic
//! [`MockSpeakerEmbedder`].

pub mod aliases;
pub mod clips;
pub mod embed;
pub mod features;
pub mod registry;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::memory::{MemoryStore, Recall};
use crate::vecmath::{cosine, l2_normalize};

pub use aliases::{AliasMap, SpeakerAliases};
pub use clips::{ClipInfo, ClipStore};
pub use embed::{MockSpeakerEmbedder, SpeakerEmbedder};
pub use registry::{Attribution, Identified, NameSource, SpeakerProfile, SpeakerRegistry};

/// Minimum accepted samples before a guided enrollment can be committed to a
/// profile (SpeakerEnrollmentPlan.md Phase 2). A few phrases give the centroid
/// enough support to be stable; fewer risks a brittle voiceprint.
pub const MIN_ENROLL_SAMPLES: usize = 3;

/// Lowest cosine a new enrollment sample may have against the running mean of the
/// already-accepted samples before it is rejected as "that didn't sound like the
/// same person" (noise, a different speaker, or a bad capture). Deliberately loose
/// — far-field captures of one person vary — so it only catches gross mismatches;
/// calibrate alongside the identification thresholds on real hardware.
pub const ENROLL_CONSISTENCY_MIN: f32 = 0.30;

/// Reserved `speaker_id` for turns we don't attribute to a specific person
/// (too little voiced audio, or speaker ID disabled). Preserves the pre-feature
/// "one shared household" behavior as the floor; memory scoped to it is shared.
pub const HOUSEHOLD_SPEAKER: &str = "household";

/// The outcome of identifying a turn's speaker: who they are and how sure we are.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerContext {
    /// Stable speaker id (`spk-…`), or [`HOUSEHOLD_SPEAKER`] when unattributed.
    pub speaker_id: String,
    /// The person's name, if their cluster has been named; `None` while anonymous.
    pub name: Option<String>,
    /// True when this utterance created a brand-new anonymous cluster.
    pub is_new: bool,
    /// Cosine similarity to the matched profile (0.0 for a new cluster/household).
    pub confidence: f32,
}

impl SpeakerContext {
    /// The unattributed/shared-household context (identification off or too short).
    pub fn household() -> Self {
        Self {
            speaker_id: HOUSEHOLD_SPEAKER.to_string(),
            name: None,
            is_new: false,
            confidence: 0.0,
        }
    }

    /// Whether this is the shared/unattributed household context.
    pub fn is_household(&self) -> bool {
        self.speaker_id == HOUSEHOLD_SPEAKER
    }
}

/// Decision thresholds for identification (speaker_id_plan.md §2). Env-tunable and
/// calibrated against real far-field audio in Phase E; these are placeholders.
#[derive(Debug, Clone, Copy)]
pub struct SpeakerThresholds {
    /// Cosine ≥ this ⇒ confident match; the profile's centroid is updated.
    pub match_threshold: f32,
    /// Cosine in `[new_threshold, match_threshold)` ⇒ attribute to the best match
    /// for this turn but **don't** mutate its centroid; below ⇒ new cluster.
    pub new_threshold: f32,
    /// Minimum utterance length (samples at 16 kHz) to attempt identification.
    /// Shorter utterances fall back to [`HOUSEHOLD_SPEAKER`] so barge-in / TTS
    /// self-triggers don't pollute clusters.
    pub min_speech_samples: usize,
}

impl Default for SpeakerThresholds {
    fn default() -> Self {
        Self {
            match_threshold: 0.55,
            new_threshold: 0.40,
            // 1200 ms × 16 samples/ms at 16 kHz.
            min_speech_samples: 1200 * 16,
        }
    }
}

impl SpeakerThresholds {
    /// Build thresholds, treating a non-positive `min_speech_ms` as the default.
    pub fn from_ms(match_threshold: f32, new_threshold: f32, min_speech_ms: u32) -> Self {
        let d = Self::default();
        Self {
            match_threshold,
            new_threshold,
            min_speech_samples: if min_speech_ms == 0 {
                d.min_speech_samples
            } else {
                min_speech_ms as usize * (embed::SAMPLE_RATE as usize / 1000)
            },
        }
    }
}

/// The outcome of offering one utterance to a guided enrollment session
/// (SpeakerEnrollmentPlan.md Phase 2). Reported back to the device so the enroll
/// screen can show progress and ask for a retry on a rejected sample.
#[derive(Debug, Clone, PartialEq)]
pub struct EnrollSampleOutcome {
    /// Whether this sample was added to the session.
    pub accepted: bool,
    /// Why a sample was rejected (`None` when accepted), for a user-facing hint.
    pub reason: Option<String>,
    /// How many samples the session holds *after* this one (0 if rejected-first).
    pub samples: usize,
    /// Cosine of this sample against the running mean of prior samples (1.0 for the
    /// first sample). A low value is what triggers an "inconsistent" rejection.
    pub consistency: f32,
    /// Whether the session now has enough samples to be committed.
    pub ready: bool,
}

/// The result of committing a guided enrollment session into a named profile.
#[derive(Debug, Clone, PartialEq)]
pub struct EnrollFinishOutcome {
    /// The new (or updated) profile's stable `speaker_id`.
    pub speaker_id: String,
    /// The name the profile was saved under.
    pub name: String,
    /// How many samples backed the committed centroid.
    pub samples: usize,
    /// `true` when the samples were folded into a person's **existing** voiceprint
    /// (a profile already carried this name); `false` when a new profile was created.
    pub updated: bool,
}

/// What a coordinated [`SpeakerService::merge`] / [`SpeakerService::delete`]
/// changed besides the voiceprint itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpeakerChange {
    /// SQLite memory rows re-attributed (merge) or made shared (delete).
    pub memories: usize,
    /// GraphRAG `Turn`/`Memory` nodes re-attributed (0 on the SQLite-only backend).
    pub graph_nodes: u64,
    /// Follow-up steps that failed after the registry change committed (memory,
    /// clips, graph), as human-readable messages. Empty on full success.
    pub warnings: Vec<String>,
}

impl SpeakerChange {
    fn warn(&mut self, message: String) {
        log::warn!("speaker change: {message}");
        self.warnings.push(message);
    }
}

/// Owns the embedder + registry + thresholds and turns an utterance into a
/// [`SpeakerContext`]. Cheap to share behind an `Arc` across concurrent turns
/// (the registry serializes its own SQLite access).
pub struct SpeakerService {
    embedder: Arc<dyn SpeakerEmbedder>,
    registry: SpeakerRegistry,
    thresholds: SpeakerThresholds,
    /// In-flight guided-enrollment sessions, keyed by the device-chosen session id:
    /// the accepted sample embeddings accumulated so far. Dropped on commit/cancel.
    enrollments: Mutex<HashMap<String, Vec<Vec<f32>>>>,
    /// Optional rolling store of the raw utterances behind each voiceprint, so a human
    /// can play a cluster's clips back to put a name to it (speaker_id_plan.md Phase C).
    /// `None` when clip capture is unavailable (e.g. the store dir couldn't be opened);
    /// identification still works, there's just nothing to play back.
    clips: Option<ClipStore>,
}

impl SpeakerService {
    /// Assemble a service from an embedder, a profile registry, and thresholds, with
    /// no raw-clip capture (clips are `None`).
    pub fn new(
        embedder: Arc<dyn SpeakerEmbedder>,
        registry: SpeakerRegistry,
        thresholds: SpeakerThresholds,
    ) -> Self {
        Self::new_with_clips(embedder, registry, thresholds, None)
    }

    /// Assemble a service that also persists a rolling window of each speaker's raw
    /// utterances into `clips` (the People screen's "play" button). Pass `None` to
    /// disable clip capture (equivalent to [`new`](Self::new)).
    pub fn new_with_clips(
        embedder: Arc<dyn SpeakerEmbedder>,
        registry: SpeakerRegistry,
        thresholds: SpeakerThresholds,
        clips: Option<ClipStore>,
    ) -> Self {
        Self {
            embedder,
            registry,
            thresholds,
            enrollments: Mutex::new(HashMap::new()),
            clips,
        }
    }

    /// The underlying registry (the Phase-C control handler lists/renames it).
    pub fn registry(&self) -> &SpeakerRegistry {
        &self.registry
    }

    /// List `speaker_id`'s stored voice clips, newest first (empty when clip capture
    /// is off or the speaker has none).
    pub fn list_clips(&self, speaker_id: &str) -> Result<Vec<ClipInfo>> {
        match &self.clips {
            Some(store) => store.list(speaker_id),
            None => Ok(Vec::new()),
        }
    }

    /// The raw little-endian `i16` PCM bytes of one of `speaker_id`'s clips. Errors
    /// when clip capture is off or the clip is unknown.
    pub fn load_clip(&self, speaker_id: &str, clip: &str) -> Result<Vec<u8>> {
        match &self.clips {
            Some(store) => store.load(speaker_id, clip),
            None => anyhow::bail!("voice-clip capture is not enabled on the orchestrator"),
        }
    }

    /// Persist one raw utterance for `speaker_id` into the rolling clip store, if
    /// enabled. Best-effort: a clip-IO failure is logged, never propagated (it must
    /// not break the turn).
    fn save_clip(&self, speaker_id: &str, pcm: &[i16]) {
        if let Some(store) = &self.clips {
            if let Err(e) = store.save(speaker_id, pcm) {
                log::warn!("saving voice clip for {speaker_id} failed: {e:#}");
            }
        }
    }

    /// Merge voice cluster `drop` into `keep` everywhere the person lives: the
    /// voiceprints (one registry transaction), their SQLite memories, their stored
    /// voice clips, and their GraphRAG history (`graph`, the live recall backend —
    /// `None` or the SQLite-only backend is a no-op there). Returns `Ok(None)` when
    /// the registry declined (unknown id, or `keep == drop`) and nothing changed.
    ///
    /// The registry merge is the commit point; the follow-ups run after it and are
    /// best-effort — a failure is logged and reported in [`SpeakerChange::warnings`]
    /// rather than undoing a merge the user asked for.
    pub async fn merge(
        &self,
        keep: &str,
        drop: &str,
        memory: &MemoryStore,
        graph: Option<&dyn Recall>,
    ) -> Result<Option<SpeakerChange>> {
        if !self.registry.merge(keep, drop)? {
            return Ok(None);
        }
        let mut change = SpeakerChange::default();
        match memory.reassign_speaker(drop, keep) {
            Ok(n) => change.memories = n,
            Err(e) => change.warn(format!("moving memories {drop}→{keep}: {e:#}")),
        }
        if let Some(store) = &self.clips {
            if let Err(e) = store.merge_speakers(keep, drop) {
                change.warn(format!("merging voice clips {drop}→{keep}: {e:#}"));
            }
        }
        if let Some(graph) = graph {
            match graph.reassign_speaker(drop, keep).await {
                Ok(n) => change.graph_nodes = n,
                Err(e) => change.warn(format!("re-attributing graph memory {drop}→{keep}: {e:#}")),
            }
        }
        Ok(Some(change))
    }

    /// Forget speaker `id`: delete their voiceprint and stored voice clips, and make
    /// their memories shared/household — in SQLite and in the GraphRAG graph — so
    /// nothing is left attributed to a person who no longer exists (the device's
    /// "Forget" dialog: memories stay but become unattributed). Returns `Ok(None)`
    /// when there is no such speaker. Follow-ups after the registry delete are
    /// best-effort, as in [`merge`](Self::merge).
    pub async fn delete(
        &self,
        id: &str,
        memory: &MemoryStore,
        graph: Option<&dyn Recall>,
    ) -> Result<Option<SpeakerChange>> {
        if !self.registry.delete(id)? {
            return Ok(None);
        }
        let mut change = SpeakerChange::default();
        match memory.unassign_speaker(id) {
            Ok(n) => change.memories = n,
            Err(e) => change.warn(format!("unassigning memories of {id}: {e:#}")),
        }
        if let Some(store) = &self.clips {
            if let Err(e) = store.delete_speaker(id) {
                change.warn(format!("deleting voice clips of {id}: {e:#}"));
            }
        }
        if let Some(graph) = graph {
            match graph.delete_speaker(id).await {
                Ok(n) => change.graph_nodes = n,
                Err(e) => change.warn(format!("detaching graph memory of {id}: {e:#}")),
            }
        }
        Ok(Some(change))
    }

    /// **Read-only** identification: the best-matching profile and its cosine score,
    /// without creating clusters or updating any centroid. Used by the enrollment
    /// "verify" step to confirm a freshly enrolled voice is recognized. `None` when
    /// there are no profiles or the best score is below `new_threshold`.
    pub fn identify_only(&self, pcm: &[i16]) -> Result<Option<(String, Option<String>, f32)>> {
        if pcm.len() < self.thresholds.min_speech_samples {
            return Ok(None);
        }
        let embedding = self.embedder.embed(pcm)?;
        match self.registry.identify(&embedding)? {
            Some((id, score)) if score >= self.thresholds.new_threshold => {
                let name = self.registry.get(&id)?.and_then(|p| p.name);
                Ok(Some((id, name, score)))
            }
            _ => Ok(None),
        }
    }

    /// Offer one utterance to the guided-enrollment session `session_id`, creating
    /// the session on first use. Rejects a too-short capture or one that is
    /// grossly inconsistent with the samples gathered so far; otherwise accumulates
    /// its embedding. Returns an [`EnrollSampleOutcome`] for the device UI.
    pub fn enroll_sample(&self, session_id: &str, pcm: &[i16]) -> Result<EnrollSampleOutcome> {
        if pcm.len() < self.thresholds.min_speech_samples {
            let samples = self
                .enrollments
                .lock()
                .unwrap()
                .get(session_id)
                .map_or(0, Vec::len);
            return Ok(EnrollSampleOutcome {
                accepted: false,
                reason: Some("too short — please speak a little longer".to_string()),
                samples,
                consistency: 0.0,
                ready: samples >= MIN_ENROLL_SAMPLES,
            });
        }
        let embedding = self.embedder.embed(pcm)?;
        let mut sessions = self.enrollments.lock().unwrap();
        let samples = sessions.entry(session_id.to_string()).or_default();

        // Consistency = cosine against the mean of the already-accepted samples.
        let consistency = if samples.is_empty() {
            1.0
        } else {
            let mean = mean_vector(samples);
            cosine(&embedding, &mean)
        };
        if !samples.is_empty() && consistency < ENROLL_CONSISTENCY_MIN {
            return Ok(EnrollSampleOutcome {
                accepted: false,
                reason: Some("that didn't sound like the earlier samples".to_string()),
                samples: samples.len(),
                consistency,
                ready: samples.len() >= MIN_ENROLL_SAMPLES,
            });
        }
        samples.push(embedding);
        Ok(EnrollSampleOutcome {
            accepted: true,
            reason: None,
            samples: samples.len(),
            consistency,
            ready: samples.len() >= MIN_ENROLL_SAMPLES,
        })
    }

    /// How many samples the session `session_id` currently holds.
    pub fn enroll_count(&self, session_id: &str) -> usize {
        self.enrollments
            .lock()
            .unwrap()
            .get(session_id)
            .map_or(0, Vec::len)
    }

    /// Commit the guided-enrollment session into a **named, labeled** profile:
    /// average the accepted sample embeddings into a centroid and store it. When a
    /// profile already carries `name` (case-insensitive) — e.g. enrolling an existing
    /// household member who was named earlier — the samples are folded into that
    /// voiceprint rather than minting a same-named duplicate. Fails (without consuming
    /// the session) when fewer than [`MIN_ENROLL_SAMPLES`] were gathered, so the device
    /// can keep collecting.
    pub fn enroll_finish(&self, session_id: &str, name: &str) -> Result<EnrollFinishOutcome> {
        let mut sessions = self.enrollments.lock().unwrap();
        let samples = sessions.get(session_id).cloned().unwrap_or_default();
        if samples.len() < MIN_ENROLL_SAMPLES {
            anyhow::bail!(
                "need at least {MIN_ENROLL_SAMPLES} voice samples (have {})",
                samples.len()
            );
        }
        let centroid = mean_vector(&samples);
        let n = samples.len() as u64;
        let existing = match self.registry.find_by_name(name)? {
            Some(id) if self.registry.absorb(&id, Some(name), &centroid, n)? => Some(id),
            _ => None,
        };
        let updated = existing.is_some();
        let id = match existing {
            Some(id) => id,
            None => self.registry.create_labeled(name, &centroid, n)?,
        };
        sessions.remove(session_id);
        Ok(EnrollFinishOutcome {
            speaker_id: id,
            name: name.trim().to_string(),
            samples: samples.len(),
            updated,
        })
    }

    /// Commit the guided-enrollment session as **extra samples for an existing
    /// profile** `speaker_id` (the People screen's "Add voice samples"): fold the
    /// accepted samples' mean into that voiceprint, keeping its name (an anonymous
    /// cluster stays anonymous). Fails without consuming the session when fewer than
    /// [`MIN_ENROLL_SAMPLES`] were gathered or the profile no longer exists.
    pub fn enroll_into(&self, session_id: &str, speaker_id: &str) -> Result<EnrollFinishOutcome> {
        let mut sessions = self.enrollments.lock().unwrap();
        let samples = sessions.get(session_id).cloned().unwrap_or_default();
        if samples.len() < MIN_ENROLL_SAMPLES {
            anyhow::bail!(
                "need at least {MIN_ENROLL_SAMPLES} voice samples (have {})",
                samples.len()
            );
        }
        let centroid = mean_vector(&samples);
        if !self
            .registry
            .absorb(speaker_id, None, &centroid, samples.len() as u64)?
        {
            anyhow::bail!("that person's voiceprint no longer exists");
        }
        sessions.remove(session_id);
        let name = self
            .registry
            .get(speaker_id)?
            .and_then(|p| p.name)
            .unwrap_or_default();
        Ok(EnrollFinishOutcome {
            speaker_id: speaker_id.to_string(),
            name,
            samples: samples.len(),
            updated: true,
        })
    }

    /// Discard an in-flight enrollment session (the user backed out).
    pub fn enroll_cancel(&self, session_id: &str) {
        self.enrollments.lock().unwrap().remove(session_id);
    }

    /// Identify (and, on a confident match, reinforce) the speaker of `pcm`.
    ///
    /// - too little audio ⇒ [`SpeakerContext::household`];
    /// - cosine ≥ `match_threshold` ⇒ known speaker, centroid updated;
    /// - `new_threshold ≤ cosine < match_threshold` ⇒ tentative match, no update;
    /// - otherwise ⇒ a new anonymous cluster is created.
    ///
    /// The match-or-create decision is one registry transaction
    /// ([`SpeakerRegistry::identify_or_create`]), so concurrent turns from the same
    /// new voice yield one cluster, not several. Blocking (mel features + ECAPA
    /// inference + SQLite + a clip write): async callers use
    /// [`identify_and_attribute_blocking`](Self::identify_and_attribute_blocking).
    pub fn identify_and_attribute(&self, pcm: &[i16]) -> Result<SpeakerContext> {
        if pcm.len() < self.thresholds.min_speech_samples {
            return Ok(SpeakerContext::household());
        }

        let embedding = self.embedder.embed(pcm)?;
        let found = self.registry.identify_or_create(
            &embedding,
            self.thresholds.match_threshold,
            self.thresholds.new_threshold,
        )?;
        // Keep this utterance in the rolling clip window so a human can play the
        // voice back: for a brand-new cluster it is exactly what's needed to put a
        // name to the anonymous "Speaker N"; a known person accumulates fresh ones.
        self.save_clip(&found.id, pcm);
        Ok(SpeakerContext {
            speaker_id: found.id,
            name: found.name,
            is_new: found.attribution == Attribution::Created,
            confidence: found.score,
        })
    }

    /// [`identify_and_attribute`](Self::identify_and_attribute) on tokio's blocking
    /// pool, so the inference + SQLite + clip I/O never stalls an async worker.
    pub async fn identify_and_attribute_blocking(
        self: &Arc<Self>,
        pcm: Vec<i16>,
    ) -> Result<SpeakerContext> {
        let svc = self.clone();
        tokio::task::spawn_blocking(move || svc.identify_and_attribute(&pcm))
            .await
            .context("joining the speaker-identification task")?
    }
}

/// L2-normalized mean of a set of (already normalized) embeddings — the centroid
/// of a cluster of enrollment samples.
fn mean_vector(vectors: &[Vec<f32>]) -> Vec<f32> {
    let dims = vectors.first().map_or(0, Vec::len);
    let mut acc = vec![0.0f32; dims];
    for v in vectors {
        for (a, x) in acc.iter_mut().zip(v) {
            *a += *x;
        }
    }
    l2_normalize(acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, ms: usize) -> Vec<i16> {
        let n = 16_000usize * ms / 1000;
        (0..n)
            .map(|t| ((std::f32::consts::TAU * hz * t as f32 / 16_000.0).sin() * 8000.0) as i16)
            .collect()
    }

    fn service() -> SpeakerService {
        SpeakerService::new(
            Arc::new(MockSpeakerEmbedder::default()),
            SpeakerRegistry::open_in_memory().unwrap(),
            SpeakerThresholds::default(),
        )
    }

    #[test]
    fn short_utterance_is_household() {
        let svc = service();
        // 300 ms < the 1200 ms floor.
        let ctx = svc.identify_and_attribute(&tone(180.0, 300)).unwrap();
        assert!(ctx.is_household());
        assert_eq!(
            svc.registry().count().unwrap(),
            0,
            "no cluster from a short blip"
        );
    }

    #[test]
    fn first_voice_creates_cluster_then_re_identifies_stably() {
        let svc = service();
        let first = svc.identify_and_attribute(&tone(180.0, 1500)).unwrap();
        assert!(first.is_new);
        assert!(!first.is_household());
        assert!(first.name.is_none());

        // Same voice returns → matched to the same cluster, not a new one.
        let again = svc.identify_and_attribute(&tone(180.0, 1600)).unwrap();
        assert!(!again.is_new);
        assert_eq!(again.speaker_id, first.speaker_id);
        assert!(again.confidence >= SpeakerThresholds::default().match_threshold);
        assert_eq!(svc.registry().count().unwrap(), 1);
    }

    #[test]
    fn two_distinct_voices_get_distinct_clusters() {
        let svc = service();
        let sam = svc.identify_and_attribute(&tone(150.0, 1500)).unwrap();
        let dana = svc.identify_and_attribute(&tone(600.0, 1500)).unwrap();
        assert_ne!(sam.speaker_id, dana.speaker_id);
        assert!(sam.is_new && dana.is_new);
        assert_eq!(svc.registry().count().unwrap(), 2);
    }

    #[test]
    fn name_flows_into_later_contexts() {
        let svc = service();
        let ctx = svc.identify_and_attribute(&tone(180.0, 1500)).unwrap();
        svc.registry().rename(&ctx.speaker_id, "Sam").unwrap();
        let later = svc.identify_and_attribute(&tone(180.0, 1500)).unwrap();
        assert_eq!(later.speaker_id, ctx.speaker_id);
        assert_eq!(later.name.as_deref(), Some("Sam"));
    }

    #[test]
    fn enrollment_accumulates_then_commits_a_named_profile() {
        let svc = service();
        // Three consistent samples of the "same voice" (same pitch, varied length).
        for (i, ms) in [1500usize, 1700, 1600].into_iter().enumerate() {
            let out = svc.enroll_sample("sess-1", &tone(180.0, ms)).unwrap();
            assert!(out.accepted, "sample {i} should be accepted");
            assert_eq!(out.samples, i + 1);
        }
        assert!(svc.enroll_count("sess-1") >= MIN_ENROLL_SAMPLES);
        let fin = svc.enroll_finish("sess-1", "Dana").unwrap();
        assert_eq!(fin.name, "Dana");
        assert_eq!(fin.samples, 3);
        assert_eq!(svc.enroll_count("sess-1"), 0, "session cleared on commit");

        // The enrolled voice is now recognized by name, read-only.
        let got = svc.identify_only(&tone(180.0, 1600)).unwrap();
        let (id, name, score) = got.expect("enrolled voice should be recognized");
        assert_eq!(id, fin.speaker_id);
        assert_eq!(name.as_deref(), Some("Dana"));
        assert!(score >= SpeakerThresholds::default().new_threshold);
        let p = svc.registry().get(&fin.speaker_id).unwrap().unwrap();
        assert!(p.labeled);
    }

    #[test]
    fn enrollment_rejects_short_and_inconsistent_samples() {
        let svc = service();
        // Too short → rejected, nothing accumulated.
        let short = svc.enroll_sample("s", &tone(180.0, 300)).unwrap();
        assert!(!short.accepted);
        assert_eq!(svc.enroll_count("s"), 0);

        // One good sample, then a wildly different voice → inconsistent rejection.
        assert!(svc.enroll_sample("s", &tone(160.0, 1500)).unwrap().accepted);
        let bad = svc.enroll_sample("s", &tone(2000.0, 1500)).unwrap();
        assert!(!bad.accepted);
        assert!(bad.consistency < ENROLL_CONSISTENCY_MIN);
        assert_eq!(svc.enroll_count("s"), 1, "bad sample not accumulated");
    }

    #[test]
    fn enroll_finish_requires_minimum_samples() {
        let svc = service();
        svc.enroll_sample("s", &tone(180.0, 1500)).unwrap();
        assert!(
            svc.enroll_finish("s", "Sam").is_err(),
            "one sample is below the floor"
        );
        assert_eq!(svc.enroll_count("s"), 1, "failed finish keeps the session");
    }

    #[test]
    fn enroll_finish_folds_into_an_existing_same_named_profile() {
        let svc = service();
        for _ in 0..MIN_ENROLL_SAMPLES {
            svc.enroll_sample("a", &tone(180.0, 1500)).unwrap();
        }
        let first = svc.enroll_finish("a", "Dana").unwrap();
        assert!(!first.updated, "first enrollment creates a profile");

        for _ in 0..MIN_ENROLL_SAMPLES {
            svc.enroll_sample("b", &tone(180.0, 1500)).unwrap();
        }
        let second = svc.enroll_finish("b", "  dana ").unwrap();
        assert!(
            second.updated,
            "same name (any case) attaches to the existing one"
        );
        assert_eq!(second.speaker_id, first.speaker_id);

        let all = svc.registry().list().unwrap();
        assert_eq!(all.len(), 1, "no duplicate profile");
        assert_eq!(all[0].samples, 2 * MIN_ENROLL_SAMPLES as u64);
    }

    #[test]
    fn enroll_into_adds_samples_to_a_specific_profile() {
        let svc = service();
        // An anonymous cluster from passive identification.
        let anon = svc.identify_and_attribute(&tone(180.0, 1500)).unwrap();
        for _ in 0..MIN_ENROLL_SAMPLES {
            svc.enroll_sample("s", &tone(180.0, 1500)).unwrap();
        }
        let out = svc.enroll_into("s", &anon.speaker_id).unwrap();
        assert!(out.updated);
        assert_eq!(out.speaker_id, anon.speaker_id);
        let p = svc.registry().get(&anon.speaker_id).unwrap().unwrap();
        assert_eq!(p.name, None, "an anonymous cluster stays anonymous");
        assert!(!p.labeled);
        assert_eq!(p.samples, 1 + MIN_ENROLL_SAMPLES as u64);
        assert_eq!(svc.enroll_count("s"), 0, "session consumed");

        // An unknown id fails and keeps the session for a retry.
        for _ in 0..MIN_ENROLL_SAMPLES {
            svc.enroll_sample("t", &tone(180.0, 1500)).unwrap();
        }
        assert!(svc.enroll_into("t", "spk-missing").is_err());
        assert_eq!(svc.enroll_count("t"), MIN_ENROLL_SAMPLES);
    }

    #[test]
    fn enroll_cancel_discards_the_session() {
        let svc = service();
        svc.enroll_sample("s", &tone(180.0, 1500)).unwrap();
        svc.enroll_cancel("s");
        assert_eq!(svc.enroll_count("s"), 0);
    }

    #[test]
    fn attribution_saves_a_playable_clip_for_the_speaker() {
        let tmp = std::env::temp_dir().join(format!(
            "anamanti-svc-clips-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let svc = SpeakerService::new_with_clips(
            Arc::new(MockSpeakerEmbedder::default()),
            SpeakerRegistry::open_in_memory().unwrap(),
            SpeakerThresholds::default(),
            Some(ClipStore::open(&tmp).unwrap()),
        );
        let utterance = tone(180.0, 1500);
        let ctx = svc.identify_and_attribute(&utterance).unwrap();

        // A clip was stored for the new cluster, and its bytes round-trip back as the
        // same PCM the "play" button would submit to the speaker.
        let clips = svc.list_clips(&ctx.speaker_id).unwrap();
        assert_eq!(clips.len(), 1, "one clip captured for the new speaker");
        let bytes = svc.load_clip(&ctx.speaker_id, &clips[0].name).unwrap();
        assert_eq!(
            bytes.len(),
            utterance.len() * 2,
            "raw i16 LE, 2 bytes/sample"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn clip_methods_degrade_gracefully_without_a_store() {
        let svc = service(); // no clip store
        assert!(svc.list_clips("spk-1").unwrap().is_empty());
        assert!(svc.load_clip("spk-1", "whatever").is_err());
    }

    /// An embedder that holds every caller at a barrier after embedding, so all of
    /// them reach the registry's identify-or-create decision at the same moment.
    struct BarrierEmbedder {
        inner: MockSpeakerEmbedder,
        barrier: std::sync::Barrier,
    }

    impl SpeakerEmbedder for BarrierEmbedder {
        fn embed(&self, pcm: &[i16]) -> Result<Vec<f32>> {
            let v = self.inner.embed(pcm);
            self.barrier.wait();
            v
        }
        fn dims(&self) -> usize {
            self.inner.dims()
        }
    }

    /// A20: concurrent turns from one new voice must create a single cluster.
    #[test]
    fn concurrent_new_voice_identifications_create_one_cluster() {
        const N: usize = 8;
        let svc = Arc::new(SpeakerService::new(
            Arc::new(BarrierEmbedder {
                inner: MockSpeakerEmbedder::default(),
                barrier: std::sync::Barrier::new(N),
            }),
            SpeakerRegistry::open_in_memory().unwrap(),
            SpeakerThresholds::default(),
        ));
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let svc = svc.clone();
                std::thread::spawn(move || svc.identify_and_attribute(&tone(180.0, 1500)).unwrap())
            })
            .collect();
        let ctxs: Vec<SpeakerContext> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(svc.registry().count().unwrap(), 1, "one voice, one cluster");
        assert_eq!(ctxs.iter().filter(|c| c.is_new).count(), 1);
        assert!(ctxs.iter().all(|c| c.speaker_id == ctxs[0].speaker_id));
    }

    #[tokio::test]
    async fn blocking_wrapper_identifies_off_the_runtime() {
        let svc = Arc::new(service());
        let ctx = svc
            .identify_and_attribute_blocking(tone(180.0, 1500))
            .await
            .unwrap();
        assert!(ctx.is_new);
        assert_eq!(svc.registry().count().unwrap(), 1);
    }

    fn tmp_clips() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "anamanti-svc-merge-{}-{}",
            std::process::id(),
            crate::clock::now_millis()
        ))
    }

    /// A8: merge moves the voiceprint, SQLite memories and clips in one call; delete
    /// drops the voiceprint and clips and makes the memories shared.
    #[tokio::test]
    async fn merge_and_delete_coordinate_registry_memory_and_clips() {
        use crate::memory::{MemoryKind, MemorySource, SqliteRecall};

        let tmp = tmp_clips();
        let svc = SpeakerService::new_with_clips(
            Arc::new(MockSpeakerEmbedder::default()),
            SpeakerRegistry::open_in_memory().unwrap(),
            SpeakerThresholds::default(),
            Some(ClipStore::open(&tmp).unwrap()),
        );
        let memory = MemoryStore::open_in_memory().unwrap();
        let keep = svc.identify_and_attribute(&tone(150.0, 1500)).unwrap();
        let drop = svc.identify_and_attribute(&tone(600.0, 1500)).unwrap();
        memory
            .add_scoped(
                MemoryKind::Fact,
                "likes tea",
                MemorySource::Explicit,
                Some(&drop.speaker_id),
            )
            .unwrap();
        // The SQLite-only backend's graph hook is a no-op.
        let fts = SqliteRecall::new(Arc::new(MemoryStore::open_in_memory().unwrap()));

        let change = svc
            .merge(&keep.speaker_id, &drop.speaker_id, &memory, Some(&fts))
            .await
            .unwrap()
            .expect("merge happened");
        assert_eq!(change.memories, 1);
        assert_eq!(change.graph_nodes, 0);
        assert!(change.warnings.is_empty(), "{:?}", change.warnings);
        assert_eq!(svc.registry().count().unwrap(), 1);
        assert!(memory
            .search_scoped("tea", Some(&keep.speaker_id), 5)
            .unwrap()
            .iter()
            .any(|m| m.content == "likes tea"));
        assert_eq!(svc.list_clips(&keep.speaker_id).unwrap().len(), 2);
        assert!(svc.list_clips(&drop.speaker_id).unwrap().is_empty());

        // Merging an id that no longer exists changes nothing.
        assert!(svc
            .merge(&keep.speaker_id, &drop.speaker_id, &memory, None)
            .await
            .unwrap()
            .is_none());

        let gone = svc
            .delete(&keep.speaker_id, &memory, None)
            .await
            .unwrap()
            .expect("delete happened");
        assert_eq!(gone.memories, 1);
        assert_eq!(svc.registry().count().unwrap(), 0);
        assert!(svc.list_clips(&keep.speaker_id).unwrap().is_empty());
        // The memory survives, now shared (visible in household scope).
        assert!(memory
            .search_scoped("tea", None, 5)
            .unwrap()
            .iter()
            .any(|m| m.content == "likes tea"));
        assert!(svc
            .delete(&keep.speaker_id, &memory, None)
            .await
            .unwrap()
            .is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A8: after a merge, the kept speaker's Helix-scoped recall — through the
    /// hot-swappable proxy's seam — finds what the dropped speaker said.
    #[test]
    fn merge_rewrites_graph_so_kept_speaker_recalls_dropped_turns() {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(merge_graph_body());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    async fn merge_graph_body() {
        use crate::memory::embed::{Embedder, MockEmbedder};
        use crate::memory::helix::{turn_text, HelixMemory, NewTurn};
        use crate::memory::HelixRecall;

        let svc = service();
        let memory = MemoryStore::open_in_memory().unwrap();
        let keep = svc.identify_and_attribute(&tone(150.0, 1500)).unwrap();
        let drop = svc.identify_and_attribute(&tone(600.0, 1500)).unwrap();

        let embedder = Arc::new(MockEmbedder::new(32));
        let helix = Arc::new(HelixMemory::open_in_memory("merge", 32).await.unwrap());
        let (transcript, reply) = ("my sister lives in Lisbon", "Lovely city.");
        helix
            .ingest_turn(NewTurn {
                ext_id: "t1",
                session: "s",
                ts: 1,
                transcript,
                reply,
                embedding: embedder
                    .embed_one(&turn_text(transcript, reply))
                    .await
                    .unwrap(),
                entities: &[],
                speaker_id: &drop.speaker_id,
                speaker_name: None,
            })
            .await
            .unwrap();
        helix.flush().await.unwrap();
        let recall: Arc<dyn Recall> = Arc::new(HelixRecall::new(helix, embedder, 4));

        let before = recall
            .recall("where does my sister live", Some(&keep.speaker_id), 8)
            .await
            .unwrap();
        assert!(!before.hits.iter().any(|h| h.contains("Lisbon")));

        let change = svc
            .merge(
                &keep.speaker_id,
                &drop.speaker_id,
                &memory,
                Some(recall.as_ref()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change.graph_nodes, 1);
        let after = recall
            .recall("where does my sister live", Some(&keep.speaker_id), 8)
            .await
            .unwrap();
        assert!(
            after.hits.iter().any(|h| h.contains("Lisbon")),
            "kept speaker should recall the dropped speaker's turn, got {:?}",
            after.hits
        );
    }
}
