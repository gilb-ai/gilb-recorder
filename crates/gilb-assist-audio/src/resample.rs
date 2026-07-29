//! Incremental resampler: arbitrary capture rates down to the 16 kHz pipeline
//! rate, keeping filter state between chunks so stream boundaries produce no
//! clicks or drift (the batch `mix_to_mono_16k_dual` in gilb-record cannot do
//! this). One instance per channel — mic and system run at independent rates.

use std::collections::VecDeque;

use anyhow::Result;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Fixed input block the inner rubato resampler consumes. Incoming chunks of
/// arbitrary length are staged in a FIFO and processed block by block.
const BLOCK: usize = 1024;

pub struct StreamResampler {
    /// None when input already arrives at the target rate.
    inner: Option<SincFixedIn<f32>>,
    fifo: VecDeque<f32>,
    block: Vec<f32>,
    ratio: f64,
    /// Sinc filter delay still to be trimmed off the head of the output, so
    /// the output timeline starts where the input did.
    skip: usize,
    consumed_in: u64,
    emitted_out: u64,
}

impl StreamResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Result<Self> {
        let ratio = f64::from(out_rate) / f64::from(in_rate);
        let inner = if in_rate == out_rate {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            Some(SincFixedIn::new(ratio, 1.0, params, BLOCK, 1)?)
        };
        let skip = inner.as_ref().map_or(0, |r| r.output_delay());
        Ok(Self {
            inner,
            fifo: VecDeque::new(),
            block: vec![0.0; BLOCK],
            ratio,
            skip,
            consumed_in: 0,
            emitted_out: 0,
        })
    }

    /// Trim the filter delay off the head, count what actually leaves.
    fn emit(&mut self, mut produced: Vec<f32>) -> Vec<f32> {
        let drop = self.skip.min(produced.len());
        if drop > 0 {
            produced.drain(..drop);
            self.skip -= drop;
        }
        self.emitted_out += produced.len() as u64;
        produced
    }

    /// Feed a chunk, get whatever full blocks resolve to. Output trails input
    /// by less than one block plus the sinc filter delay.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<f32>> {
        self.consumed_in += samples.len() as u64;
        if self.inner.is_none() {
            return Ok(samples.to_vec());
        }
        self.fifo.extend(samples);
        let mut out = Vec::new();
        while self.fifo.len() >= BLOCK {
            for slot in self.block.iter_mut() {
                *slot = self.fifo.pop_front().unwrap();
            }
            let produced = self.inner.as_mut().unwrap().process(&[&self.block], None)?;
            out.extend(self.emit(produced.into_iter().next().unwrap()));
        }
        Ok(out)
    }

    /// Drain the staged remainder and the filter tail. Call once, at the end
    /// of the stream. Output is capped at round(input × ratio): rubato pads
    /// the last partial block with zeros, and that padding is not signal.
    pub fn flush(&mut self) -> Result<Vec<f32>> {
        if self.inner.is_none() {
            return Ok(Vec::new());
        }
        let rest: Vec<f32> = self.fifo.drain(..).collect();
        let mut out = Vec::new();
        if !rest.is_empty() {
            let produced = self
                .inner
                .as_mut()
                .unwrap()
                .process_partial(Some(&[&rest]), None)?;
            out.extend(self.emit(produced.into_iter().next().unwrap()));
        }
        let tail = self.inner.as_mut().unwrap().process_partial::<&[f32]>(None, None)?;
        out.extend(self.emit(tail.into_iter().next().unwrap()));

        let expected = (self.consumed_in as f64 * self.ratio).round() as u64;
        let excess = self.emitted_out.saturating_sub(expected) as usize;
        out.truncate(out.len().saturating_sub(excess));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn sine(rate: usize, hz: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|n| (TAU * hz * n as f32 / rate as f32).sin())
            .collect()
    }

    fn positive_zero_crossings(samples: &[f32]) -> usize {
        samples.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count()
    }

    /// 48 kHz → 16 kHz across uneven chunk sizes: total length matches the
    /// ratio and the tone survives without seams.
    #[test]
    fn downsamples_48k_stream_without_seams() {
        let input = sine(48_000, 440.0, 2.0);
        let mut rs = StreamResampler::new(48_000, 16_000).unwrap();

        let mut out = Vec::new();
        let mut pos = 0;
        // Deliberately awkward chunk sizes to exercise the FIFO staging.
        for (i, size) in [480usize, 333, 1024, 71, 4096].iter().cycle().enumerate() {
            if pos >= input.len() {
                break;
            }
            let end = (pos + size + i % 7).min(input.len());
            out.extend(rs.push(&input[pos..end]).unwrap());
            pos = end;
        }
        out.extend(rs.flush().unwrap());

        let expected = input.len() / 3;
        assert!(
            (out.len() as i64 - expected as i64).unsigned_abs() < 32,
            "expected ~{expected} samples, got {}",
            out.len()
        );

        // A clean 440 Hz tone has one positive-going zero crossing per period;
        // seams between chunks would add spurious ones. Judge the steady-state
        // middle — the filter's edge transients ring around zero.
        let mid = &out[500..out.len() - 500];
        let crossings = positive_zero_crossings(mid);
        let expected_crossings = 440 * mid.len() / 16_000;
        assert!(
            (crossings as i64 - expected_crossings as i64).unsigned_abs() <= 5,
            "expected ~{expected_crossings} zero crossings, got {crossings}"
        );
    }

    /// Non-integer ratio (44.1 kHz mic) also lands on the right length.
    #[test]
    fn downsamples_44k1() {
        let input = sine(44_100, 300.0, 1.0);
        let mut rs = StreamResampler::new(44_100, 16_000).unwrap();
        let mut out = rs.push(&input).unwrap();
        out.extend(rs.flush().unwrap());
        assert!(
            (out.len() as i64 - 16_000).unsigned_abs() < 32,
            "expected ~16000 samples, got {}",
            out.len()
        );
    }

    /// Same-rate input is passed through untouched.
    #[test]
    fn passthrough_at_target_rate() {
        let input = sine(16_000, 200.0, 0.5);
        let mut rs = StreamResampler::new(16_000, 16_000).unwrap();
        let out = rs.push(&input).unwrap();
        assert_eq!(out, input);
        assert!(rs.flush().unwrap().is_empty());
    }
}
