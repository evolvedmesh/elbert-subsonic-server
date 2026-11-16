//! What the `transcode` feature needs of audio: reading a file's headers, a WAV header, and
//! a streaming resampler. Self-contained on purpose: this tree must not reach into the app
//! that embeds it. (Elbert keeps its own copies in `stream` and `analysis`, which it also
//! uses for casting and for the aurora's analysis.)

use std::f64::consts::PI;
use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::CODEC_TYPE_NULL;
use symphonia::core::codecs::{
    CODEC_TYPE_AAC, CODEC_TYPE_ALAC, CODEC_TYPE_FLAC, CODEC_TYPE_MP3, CODEC_TYPE_OPUS,
    CODEC_TYPE_VORBIS,
};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// How a file reaches a listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Served as it is, with this content type.
    Passthrough(&'static str),
    /// Decoded and served as WAV.
    Wav,
    /// Nothing here can make it playable (WMA, which symphonia can't read).
    Unsupported,
}

/// What `probe` learns from a file's headers.
#[derive(Clone, Debug)]
pub struct Probe {
    /// How the file can be delivered.
    pub delivery: Delivery,
    /// Whether the core can decode it (and so serve it as WAV on request).
    pub decodable: bool,
    /// The sample rate, in Hz.
    pub sample_rate: u32,
    /// The number of channels.
    pub channels: u16,
    /// Bits per sample.
    pub bits: u16,
}

fn open_format(path: &Path) -> Result<(Box<dyn FormatReader>, u32), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension() {
        hint.with_extension(&ext.to_string_lossy());
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("unsupported: {e}"))?;
    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("unsupported: no audio track")?;
    let id = track.id;
    Ok((format, id))
}

/// Reads only the headers: cheap enough to run per request.
pub fn probe(path: &Path) -> Probe {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let Ok((format, track_id)) = open_format(path) else {
        return Probe {
            delivery: Delivery::Unsupported,
            decodable: false,
            sample_rate: 0,
            channels: 0,
            bits: 0,
        };
    };
    let params = &format
        .tracks()
        .iter()
        .find(|t| t.id == track_id)
        .unwrap()
        .codec_params;
    let codec = params.codec;
    let pcm = symphonia::default::get_codecs()
        .get_codec(codec)
        .is_some_and(|d| d.short_name.starts_with("pcm"));
    let mp4 = matches!(ext.as_str(), "m4a" | "mp4" | "alac" | "aac" | "m4b");
    let delivery = if codec == CODEC_TYPE_MP3 {
        Delivery::Passthrough("audio/mpeg")
    } else if codec == CODEC_TYPE_AAC {
        Delivery::Passthrough(if mp4 { "audio/mp4" } else { "audio/aac" })
    } else if codec == CODEC_TYPE_FLAC {
        Delivery::Passthrough(if mp4 { "audio/mp4" } else { "audio/flac" })
    } else if codec == CODEC_TYPE_VORBIS || codec == CODEC_TYPE_OPUS {
        Delivery::Passthrough("audio/ogg")
    } else if pcm && ext == "wav" {
        Delivery::Passthrough("audio/wav")
    } else if codec == CODEC_TYPE_ALAC || pcm {
        // ALAC, and PCM in AIFF/CAF — neither plays in a browser.
        Delivery::Wav
    } else {
        Delivery::Unsupported
    };
    Probe {
        delivery,
        decodable: delivery != Delivery::Unsupported && codec != CODEC_TYPE_OPUS,
        sample_rate: params.sample_rate.unwrap_or(0),
        channels: params.channels.map(|c| c.count() as u16).unwrap_or(2),
        bits: params
            .bits_per_sample
            .or(params.bits_per_coded_sample)
            .unwrap_or(16) as u16,
    }
}

// ---- WAV ---------------------------------------------------------------------

/// The length of the header [`wav_header`] writes.
pub const WAV_HEADER_LEN: u64 = 44;

/// A canonical 44-byte PCM header. Unknown sizes are written as the
/// maximum, which players read as "until the stream ends".
pub fn wav_header(rate: u32, channels: u16, bits: u16, frames: Option<u64>) -> Vec<u8> {
    let block = channels as u32 * (bits as u32 / 8);
    let data = frames.map(|f| f * block as u64);
    let data32 = data
        .map(|d| d.min(u32::MAX as u64 - 36) as u32)
        .unwrap_or(u32::MAX);
    let riff = if data.is_some() {
        data32 + 36
    } else {
        u32::MAX
    };
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&riff.to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * block).to_le_bytes());
    h.extend_from_slice(&(block as u16).to_le_bytes());
    h.extend_from_slice(&bits.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data32.to_le_bytes());
    h
}

// ---- Resampling ---------------------------------------------------------------

/// A windowed-sinc resampler (Kaiser-ish Blackman window, 16 zero crossings a
/// side, 512 phases), streaming: input in any blocks, output as soon as it is
/// determined. Good to well under the analyser's 16-bit floor, which is all
/// the analysis needs — it measures band energies and onsets, not fidelity.
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Kernel half-width in input samples.
    half: usize,
    table: Vec<f32>,
    taps: usize,
    passthrough: bool,
    buf: Vec<f32>,
    /// Absolute index of `buf[0]` in the input stream.
    base: i64,
    /// Output samples produced so far.
    produced: u64,
}

const PHASES: usize = 512;
const ZERO_CROSSINGS: f64 = 16.0;

impl Resampler {
    /// A resampler from `input_rate` to `output_rate` (both in Hz).
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        let step = input_rate as f64 / output_rate as f64;
        // Cut-off in cycles per input sample, a little under the output's
        // Nyquist when downsampling.
        let cutoff = 0.5 * 0.94 * (output_rate as f64 / input_rate as f64).min(1.0);
        let half = (ZERO_CROSSINGS / (2.0 * cutoff)).ceil() as usize;
        let taps = 2 * half + 1;
        let mut table = vec![0f32; (PHASES + 1) * taps];
        for phase in 0..=PHASES {
            let frac = phase as f64 / PHASES as f64;
            let mut sum = 0.0;
            for t in 0..taps {
                let x = t as f64 - half as f64 - frac;
                let v = sinc(2.0 * cutoff * x) * 2.0 * cutoff * window(x / (half as f64 + 1.0));
                table[phase * taps + t] = v as f32;
                sum += v;
            }
            // Unity gain at DC for every phase.
            if sum.abs() > 1e-12 {
                for t in 0..taps {
                    table[phase * taps + t] /= sum as f32;
                }
            }
        }
        Self {
            step,
            half,
            table,
            taps,
            passthrough: input_rate == output_rate,
            buf: Vec::new(),
            base: -(half as i64),
            produced: 0,
        }
    }

    /// Feeds `input` (interleaved samples) and appends what is ready to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        if self.buf.is_empty() {
            // Zero history before the first sample.
            self.buf.resize(self.half, 0.0);
        }
        self.buf.extend_from_slice(input);
        self.drain(out, false);
    }

    /// Flushes the tail into `out`.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.passthrough {
            return;
        }
        let pad = self.half + 2;
        self.buf.extend(std::iter::repeat_n(0.0, pad));
        self.drain(out, true);
    }

    fn drain(&mut self, out: &mut Vec<f32>, last: bool) {
        let available_end = self.base + self.buf.len() as i64; // exclusive
        loop {
            let pos = self.produced as f64 * self.step;
            let centre = pos.floor() as i64;
            let frac = pos - centre as f64;
            let first = centre - self.half as i64;
            let end = first + self.taps as i64;
            if end > available_end {
                break;
            }
            if last && centre >= available_end - (self.half as i64 + 2) {
                break;
            }
            let phase = (frac * PHASES as f64).round() as usize;
            let kernel = &self.table[phase * self.taps..(phase + 1) * self.taps];
            let start = (first - self.base) as usize;
            let window = &self.buf[start..start + self.taps];
            let mut acc = 0f32;
            for (a, b) in window.iter().zip(kernel) {
                acc += a * b;
            }
            out.push(acc);
            self.produced += 1;
        }
        // Drop what no future output can reach.
        let next_first = (self.produced as f64 * self.step).floor() as i64 - self.half as i64;
        let drop = (next_first - self.base).max(0) as usize;
        if drop > 0 && drop <= self.buf.len() {
            self.buf.drain(..drop);
            self.base += drop as i64;
        }
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Blackman over -1..1.
fn window(t: f64) -> f64 {
    if t.abs() >= 1.0 {
        return 0.0;
    }
    let u = (t + 1.0) / 2.0;
    0.42 - 0.5 * (2.0 * PI * u).cos() + 0.08 * (4.0 * PI * u).cos()
}
