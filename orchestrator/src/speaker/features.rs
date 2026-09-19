//! Log-mel filterbank front-end for the ONNX speaker embedder (speaker_id_plan.md
//! Phase E). Pure Rust, no external deps — so it builds offline and is unit-
//! testable independently of any model.
//!
//! Most ECAPA-TDNN / x-vector ONNX exports consume 80-dim log-mel "fbank" features
//! (frames × mels) rather than raw waveform. This module produces exactly that
//! from 16 kHz mono `i16` PCM: pre-emphasis → framing (Hamming) → power spectrum
//! (radix-2 FFT) → triangular mel filterbank → natural log.
//!
//! The exact parameters (window, mels, normalization) must match whatever the
//! chosen model was trained with; [`FbankConfig`] makes them explicit so they can
//! be tuned to the model in Phase E without touching the math.

/// Filterbank parameters. Defaults follow the common Kaldi/SpeechBrain 80-mel,
/// 25 ms / 10 ms configuration used by most ECAPA exports.
#[derive(Debug, Clone, Copy)]
pub struct FbankConfig {
    pub sample_rate: u32,
    /// FFT size (power of two ≥ window length).
    pub n_fft: usize,
    /// Analysis window length in samples (25 ms @ 16 kHz = 400).
    pub win_len: usize,
    /// Hop between frames in samples (10 ms @ 16 kHz = 160).
    pub hop: usize,
    /// Number of mel bands.
    pub n_mels: usize,
    pub fmin: f32,
    pub fmax: f32,
    /// Pre-emphasis coefficient (0 disables).
    pub preemphasis: f32,
}

impl Default for FbankConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16_000,
            n_fft: 512,
            win_len: 400,
            hop: 160,
            n_mels: 80,
            fmin: 20.0,
            fmax: 7600.0,
            preemphasis: 0.97,
        }
    }
}

/// Computed log-mel features: `frames` rows of `n_mels` values (row-major).
pub struct Fbank {
    pub n_mels: usize,
    pub frames: usize,
    /// `frames * n_mels` values, row-major (frame-major).
    pub data: Vec<f32>,
}

/// Compute log-mel filterbank features for `pcm` (mono `i16` at `cfg.sample_rate`).
/// Returns zero frames when the input is shorter than one window.
pub fn log_mel_fbank(pcm: &[i16], cfg: &FbankConfig) -> Fbank {
    let mel = mel_filterbank(cfg);
    let window = hamming(cfg.win_len);
    let n_bins = cfg.n_fft / 2 + 1;

    // Pre-emphasis on a working float copy.
    let mut sig: Vec<f32> = pcm.iter().map(|&s| s as f32).collect();
    if cfg.preemphasis != 0.0 {
        for i in (1..sig.len()).rev() {
            sig[i] -= cfg.preemphasis * sig[i - 1];
        }
    }

    let frames = if sig.len() < cfg.win_len {
        0
    } else {
        1 + (sig.len() - cfg.win_len) / cfg.hop
    };
    let mut data = vec![0.0f32; frames * cfg.n_mels];

    let mut re = vec![0.0f32; cfg.n_fft];
    let mut im = vec![0.0f32; cfg.n_fft];
    for f in 0..frames {
        let start = f * cfg.hop;
        // Windowed frame, zero-padded to n_fft.
        for (i, slot) in re.iter_mut().enumerate() {
            *slot = if i < cfg.win_len {
                sig[start + i] * window[i]
            } else {
                0.0
            };
        }
        im.iter_mut().for_each(|x| *x = 0.0);
        fft_in_place(&mut re, &mut im);

        // Power spectrum over the non-redundant bins.
        let power: Vec<f32> = (0..n_bins).map(|k| re[k] * re[k] + im[k] * im[k]).collect();

        // Apply mel filters → log.
        for m in 0..cfg.n_mels {
            let filt = &mel[m];
            let mut e = 0.0f32;
            for (k, &w) in filt.iter().enumerate() {
                if w != 0.0 {
                    e += w * power[k];
                }
            }
            data[f * cfg.n_mels + m] = (e + 1e-6).ln();
        }
    }

    Fbank {
        n_mels: cfg.n_mels,
        frames,
        data,
    }
}

fn hamming(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.54 - 0.46 * (std::f32::consts::TAU * i as f32 / (n as f32 - 1.0)).cos())
        .collect()
}

fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}
fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10f32.powf(mel / 2595.0) - 1.0)
}

/// Triangular mel filterbank: `n_mels` filters over `n_fft/2+1` FFT bins.
fn mel_filterbank(cfg: &FbankConfig) -> Vec<Vec<f32>> {
    let n_bins = cfg.n_fft / 2 + 1;
    let mel_min = hz_to_mel(cfg.fmin);
    let mel_max = hz_to_mel(cfg.fmax);
    // n_mels+2 equally spaced mel points → triangle edges.
    let points: Vec<f32> = (0..cfg.n_mels + 2)
        .map(|i| {
            let mel = mel_min + (mel_max - mel_min) * i as f32 / (cfg.n_mels as f32 + 1.0);
            mel_to_hz(mel)
        })
        .collect();
    // FFT-bin index for each edge frequency.
    let bin = |hz: f32| ((cfg.n_fft as f32 + 1.0) * hz / cfg.sample_rate as f32).floor() as usize;
    let edges: Vec<usize> = points.iter().map(|&hz| bin(hz).min(n_bins - 1)).collect();

    let mut fb = vec![vec![0.0f32; n_bins]; cfg.n_mels];
    for m in 0..cfg.n_mels {
        let (l, c, r) = (edges[m], edges[m + 1], edges[m + 2]);
        // Rising edge l..c, then falling edge c..r. Empty ranges (collapsed low-
        // frequency triangles) simply contribute nothing — no div-by-zero.
        for (k, slot) in fb[m].iter_mut().enumerate().take(c).skip(l) {
            *slot = (k - l) as f32 / (c - l) as f32;
        }
        for (k, slot) in fb[m].iter_mut().enumerate().take(r).skip(c) {
            *slot = (r - k) as f32 / (r - c) as f32;
        }
    }
    fb
}

/// In-place iterative radix-2 Cooley–Tukey FFT. `re`/`im` length must be a power
/// of two. Real input → set `im` to zeros before calling.
fn fft_in_place(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());
    debug_assert_eq!(im.len(), n);

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    // Butterflies.
    let mut len = 2;
    while len <= n {
        let ang = -std::f32::consts::TAU / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let a = i + k;
                let b = i + k + len / 2;
                let tr = cr * re[b] - ci * im[b];
                let ti = cr * im[b] + ci * re[b];
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_of_sine_peaks_at_expected_bin() {
        // A pure tone at bin 8 of a 64-point FFT should concentrate energy there.
        let n = 64;
        let bin = 8;
        let mut re: Vec<f32> = (0..n)
            .map(|t| (std::f32::consts::TAU * bin as f32 * t as f32 / n as f32).sin())
            .collect();
        let mut im = vec![0.0f32; n];
        fft_in_place(&mut re, &mut im);
        let power: Vec<f32> = (0..n).map(|k| re[k] * re[k] + im[k] * im[k]).collect();
        let peak = (1..n / 2)
            .max_by(|&a, &b| power[a].partial_cmp(&power[b]).unwrap())
            .unwrap();
        assert_eq!(peak, bin, "FFT peak should land at the tone's bin");
    }

    #[test]
    fn fbank_shape_is_frames_by_mels_and_finite() {
        let cfg = FbankConfig::default();
        // 1 s of a 220 Hz tone.
        let pcm: Vec<i16> = (0..16_000)
            .map(|t| ((std::f32::consts::TAU * 220.0 * t as f32 / 16_000.0).sin() * 6000.0) as i16)
            .collect();
        let fb = log_mel_fbank(&pcm, &cfg);
        assert_eq!(fb.n_mels, 80);
        assert!(fb.frames > 90 && fb.frames < 110, "≈100 frames for 1 s @10ms hop");
        assert_eq!(fb.data.len(), fb.frames * fb.n_mels);
        assert!(fb.data.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn short_input_yields_no_frames() {
        let fb = log_mel_fbank(&[0i16; 100], &FbankConfig::default());
        assert_eq!(fb.frames, 0);
        assert!(fb.data.is_empty());
    }

    #[test]
    fn mel_filterbank_covers_bins_without_panicking() {
        let cfg = FbankConfig::default();
        let fb = mel_filterbank(&cfg);
        assert_eq!(fb.len(), cfg.n_mels);
        assert_eq!(fb[0].len(), cfg.n_fft / 2 + 1);
        // Most filters carry weight. A few of the lowest-frequency triangles can
        // collapse onto a single FFT bin at n_fft=512 (finer mel spacing than the
        // bin resolution down low) — expected, and tuned to the model in Phase E.
        let non_empty = fb.iter().filter(|f| f.iter().any(|&w| w > 0.0)).count();
        assert!(
            non_empty >= cfg.n_mels - 12,
            "most mel filters should carry weight ({non_empty}/{})",
            cfg.n_mels
        );
        // The upper half (wide, high-frequency triangles) always carry weight.
        assert!(fb[cfg.n_mels / 2..]
            .iter()
            .all(|f| f.iter().any(|&w| w > 0.0)));
    }
}
