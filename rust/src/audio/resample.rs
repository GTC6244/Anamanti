//! Sample-rate conversion from the capture device's native rate to the 16 kHz
//! the wake-word pipeline requires (Plan.MD §3, Phase 2).
//!
//! Mic arrays rarely hand us exactly 16 kHz — coreaudio on the host is typically
//! 44.1/48 kHz, and the Echo Show's AAudio input may differ too. We downmix to
//! mono *in the capture callback* (cheap, allocation-free) and push device-rate
//! mono `i16` into the ring buffer; this resampler then runs on the inference
//! thread (off the real-time path), converting to 16 kHz `f32` for the model.
//!
//! A straight linear interpolator is plenty here: wake-word models are robust to
//! the mild spectral tilt it introduces, and it is O(n) with no allocation on
//! the steady-state path beyond a small carry buffer that holds the fractional
//! remainder between blocks so resampling stays seamless across callbacks.

/// Stateful linear resampler for a single mono channel.
pub struct Resampler {
    /// Input samples consumed per output sample (`src_rate / dst_rate`).
    step: f64,
    /// Fractional read position within `carry` for the next output sample.
    pos: f64,
    /// Leftover input samples spanning block boundaries, so interpolation is
    /// continuous from one capture callback to the next.
    carry: Vec<f32>,
    /// True when source and destination rates match — lets us skip interpolation.
    passthrough: bool,
}

impl Resampler {
    /// Create a resampler from `src_rate` to `dst_rate` (both in Hz).
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        let src = src_rate.max(1) as f64;
        let dst = dst_rate.max(1) as f64;
        Self {
            step: src / dst,
            pos: 0.0,
            carry: Vec::new(),
            passthrough: src_rate == dst_rate,
        }
    }

    /// Feed a block of mono input samples and append the resampled 16 kHz output
    /// to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }

        self.carry.extend_from_slice(input);

        // Emit output samples while we can interpolate between two real inputs.
        while (self.pos as usize) + 1 < self.carry.len() {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let a = self.carry[i];
            let b = self.carry[i + 1];
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }

        // Drop fully consumed input, keeping the sample the next output still
        // needs to interpolate from, and rebase the fractional position.
        let consumed = self.pos as usize;
        if consumed > 0 {
            self.carry.drain(0..consumed);
            self.pos -= consumed as f64;
        }
    }
}
