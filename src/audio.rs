//! Resampling (rubato 5.x) i pomocnicze operacje na próbkach.

use anyhow::Result;
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

/// Strumieniowy resampler mono o stałym wejściu (FixedSync::Input).
/// Tor: 48 kHz (przechwyt) → 16 kHz (whisper/VAD).
pub struct StreamResampler {
    fft: Fft<f32>,
    input: Vec<f32>,
    output: Vec<f32>,
}

impl StreamResampler {
    pub fn new(rate_in: usize, rate_out: usize, chunk: usize) -> Result<Self> {
        let fft = Fft::<f32>::new(rate_in, rate_out, chunk, 1, FixedSync::Input)?;
        let out_max = fft.output_frames_max();
        Ok(Self {
            fft,
            input: vec![0.0; chunk],
            output: vec![0.0; out_max],
        })
    }

    /// Ile próbek trzeba dostarczyć do kolejnego wywołania `process`.
    pub fn need(&self) -> usize {
        self.fft.input_frames_next()
    }

    /// Bufor wejściowy do wypełnienia dokładnie `need()` próbkami.
    pub fn input_buf(&mut self) -> &mut [f32] {
        let n = self.fft.input_frames_next();
        &mut self.input[..n]
    }

    /// Przetwarza wypełniony bufor wejściowy; zwraca przetworzone próbki.
    pub fn process(&mut self) -> Result<&[f32]> {
        let need = self.fft.input_frames_next();
        let ia = InterleavedSlice::new(&self.input[..need], 1, need)?;
        let out_max = self.fft.output_frames_max();
        let mut oa = InterleavedSlice::new_mut(&mut self.output[..out_max], 1, out_max)?;
        let (_read, written) = self.fft.process_into_buffer(&ia, &mut oa, None)?;
        Ok(&self.output[..written])
    }
}

/// Resampling całego klipu naraz (wyjście pipera 22050 Hz → 48 kHz).
/// `process_all_into_buffer` samo iteruje i przycina opóźnienie startowe + ogon.
pub struct ClipResampler {
    fft: Fft<f32>,
    rate_in: usize,
    rate_out: usize,
}

impl ClipResampler {
    pub fn new(rate_in: usize, rate_out: usize) -> Result<Self> {
        let fft = Fft::<f32>::new(rate_in, rate_out, 1024, 1, FixedSync::Input)?;
        Ok(Self {
            fft,
            rate_in,
            rate_out,
        })
    }

    pub fn resample(&mut self, clip: &[f32]) -> Result<Vec<f32>> {
        if clip.is_empty() {
            return Ok(Vec::new());
        }
        self.fft.reset();
        let max_out = clip.len() * self.rate_out / self.rate_in + self.fft.output_frames_max();
        let mut out = vec![0.0f32; max_out];
        let ia = InterleavedSlice::new(clip, 1, clip.len())?;
        let mut oa = InterleavedSlice::new_mut(&mut out[..], 1, max_out)?;
        let (_consumed, written) =
            self.fft
                .process_all_into_buffer(&ia, &mut oa, clip.len(), None)?;
        out.truncate(written);
        Ok(out)
    }
}

/// Zrzut mono f32 do WAV — do debugowania toru.
#[allow(dead_code)]
pub fn dump_wav(path: &str, rate: u32, data: &[f32]) -> Result<(), hound::Error> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in data {
        w.write_sample(s)?;
    }
    w.finalize()
}
