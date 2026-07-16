//! Speech-band detection filter: 2nd-order Butterworth high-pass (300 Hz) +
//! low-pass (3400 Hz), RBJ biquads. Applied to the DETECTION path only —
//! never to the stored/displayed/recorded audio — so background rumble
//! (fans, HVAC), mains hum, and broadband hiss stop triggering activity
//! blocks while real speech is unaffected.
//!
//! Group delay in the passband is well under 1 ms and constant, so block
//! edges (and therefore all metrics) shift by a negligible, deterministic
//! amount. Toggleable at runtime.

pub const HIGHPASS_HZ: f64 = 300.0;
pub const LOWPASS_HZ: f64 = 3400.0;

struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn highpass(sr: f64, fc: f64, q: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * fc / sr;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 + cos) / 2.0) / a0,
            b1: (-(1.0 + cos)) / a0,
            b2: ((1.0 + cos) / 2.0) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn lowpass(sr: f64, fc: f64, q: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * fc / sr;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 - cos) / 2.0) / a0,
            b1: (1.0 - cos) / a0,
            b2: ((1.0 - cos) / 2.0) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

pub struct SpeechBandFilter {
    hp: Biquad,
    lp: Biquad,
    sample_rate: f64,
}

impl SpeechBandFilter {
    pub fn new(sample_rate: f64) -> Self {
        const Q: f64 = std::f64::consts::FRAC_1_SQRT_2; // Butterworth
        Self {
            hp: Biquad::highpass(sample_rate, HIGHPASS_HZ, Q),
            lp: Biquad::lowpass(sample_rate, LOWPASS_HZ, Q),
            sample_rate,
        }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Filter `input` into `out` (cleared first). Filter state carries over
    /// between calls, so consecutive buffers form one continuous stream.
    pub fn process_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        out.reserve(input.len());
        for &s in input {
            let y = self.lp.process(self.hp.process(s as f64));
            out.push(y as f32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(v: &[f32]) -> f64 {
        (v.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
    }

    fn sine(sr: f64, hz: f64, secs: f64) -> Vec<f32> {
        (0..(sr * secs) as usize)
            .map(|i| (2.0 * std::f64::consts::PI * hz * i as f64 / sr).sin() as f32)
            .collect()
    }

    fn filtered_rms(sr: f64, hz: f64) -> f64 {
        let input = sine(sr, hz, 1.0);
        let mut f = SpeechBandFilter::new(sr);
        let mut out = Vec::new();
        f.process_into(&input, &mut out);
        // skip the settling transient
        rms(&out[out.len() / 4..])
    }

    #[test]
    fn rejects_rumble_and_hum() {
        let sr = 48_000.0;
        assert!(filtered_rms(sr, 60.0) < 0.05, "60 Hz should be rejected");
        assert!(filtered_rms(sr, 120.0) < 0.15, "120 Hz should be strongly attenuated");
    }

    #[test]
    fn passes_speech_band() {
        let sr = 48_000.0;
        for hz in [500.0, 1000.0, 2000.0] {
            let r = filtered_rms(sr, hz);
            let unity = std::f64::consts::FRAC_1_SQRT_2; // sine RMS
            assert!(
                (r / unity) > 0.75 && (r / unity) < 1.15,
                "{hz} Hz should pass ~unity, got ratio {}",
                r / unity
            );
        }
    }

    #[test]
    fn attenuates_hiss() {
        let sr = 48_000.0;
        assert!(filtered_rms(sr, 10_000.0) < 0.2, "10 kHz should be attenuated");
    }

    #[test]
    fn state_continuous_across_buffers() {
        let sr = 48_000.0;
        let input = sine(sr, 1000.0, 0.5);
        let mut whole = Vec::new();
        SpeechBandFilter::new(sr).process_into(&input, &mut whole);
        let mut chunked = Vec::new();
        let mut f = SpeechBandFilter::new(sr);
        let mut tmp = Vec::new();
        for c in input.chunks(480) {
            f.process_into(c, &mut tmp);
            chunked.extend_from_slice(&tmp);
        }
        for (a, b) in whole.iter().zip(chunked.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }
}
