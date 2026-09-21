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

pub mod embed;
pub mod features;
pub mod registry;

use std::sync::Arc;

use anyhow::Result;

pub use embed::{MockSpeakerEmbedder, SpeakerEmbedder};
pub use registry::{SpeakerProfile, SpeakerRegistry};

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

/// Owns the embedder + registry + thresholds and turns an utterance into a
/// [`SpeakerContext`]. Cheap to share behind an `Arc` across concurrent turns
/// (the registry serializes its own SQLite access).
pub struct SpeakerService {
    embedder: Arc<dyn SpeakerEmbedder>,
    registry: SpeakerRegistry,
    thresholds: SpeakerThresholds,
}

impl SpeakerService {
    /// Assemble a service from an embedder, a profile registry, and thresholds.
    pub fn new(
        embedder: Arc<dyn SpeakerEmbedder>,
        registry: SpeakerRegistry,
        thresholds: SpeakerThresholds,
    ) -> Self {
        Self {
            embedder,
            registry,
            thresholds,
        }
    }

    /// The underlying registry (the Phase-C control handler lists/renames it).
    pub fn registry(&self) -> &SpeakerRegistry {
        &self.registry
    }

    /// Identify (and, on a confident match, reinforce) the speaker of `pcm`.
    ///
    /// - too little audio ⇒ [`SpeakerContext::household`];
    /// - cosine ≥ `match_threshold` ⇒ known speaker, centroid updated;
    /// - `new_threshold ≤ cosine < match_threshold` ⇒ tentative match, no update;
    /// - otherwise ⇒ a new anonymous cluster is created.
    pub fn identify_and_attribute(&self, pcm: &[i16]) -> Result<SpeakerContext> {
        if pcm.len() < self.thresholds.min_speech_samples {
            return Ok(SpeakerContext::household());
        }

        let embedding = self.embedder.embed(pcm)?;
        let best = self.registry.identify(&embedding)?;

        match best {
            Some((id, score)) if score >= self.thresholds.match_threshold => {
                self.registry.update_centroid(&id, &embedding)?;
                let name = self.registry.get(&id)?.and_then(|p| p.name);
                Ok(SpeakerContext {
                    speaker_id: id,
                    name,
                    is_new: false,
                    confidence: score,
                })
            }
            Some((id, score)) if score >= self.thresholds.new_threshold => {
                // Borderline: attribute this turn but don't poison the voiceprint.
                let name = self.registry.get(&id)?.and_then(|p| p.name);
                Ok(SpeakerContext {
                    speaker_id: id,
                    name,
                    is_new: false,
                    confidence: score,
                })
            }
            _ => {
                let id = self.registry.create_cluster(&embedding)?;
                Ok(SpeakerContext {
                    speaker_id: id,
                    name: None,
                    is_new: true,
                    confidence: 0.0,
                })
            }
        }
    }
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
}
