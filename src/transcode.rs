//! Serving audio: the file as it is whenever that will do, or decoded and
//! re-encoded when a client asks for another format, a lower bit rate or a
//! start part-way (`stream`, the `transcoding` and `transcodeOffset` extensions).
//!
//! Everything is encoded as it is sent, at many times real time, so a client
//! waits for nothing. Every encoder is pure Rust, so this runs the same on a
//! phone. A transcode has no ranges: a client seeks in one with `timeOffset`.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response};
use rusty_mp3::{Mp3Encoder, Mp3EncoderConfig};
use rusty_opus::{Application, OpusEncoder};
use serde_json::{json, Value};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use super::audio::{self, Delivery, Probe, Resampler};
use super::body::{channel_body, empty, Body};
use super::media::{find_file, Source};
use super::Http;
use elbert_subsonic_core::api::{Answer, Params};
use elbert_subsonic_core::auth::Who;
use elbert_subsonic_core::backend::TranscodeFormat;
use elbert_subsonic_core::catalog::suffix_of;
use elbert_subsonic_core::respond::{ApiError, ERR_GENERIC};
use elbert_subsonic_core::Server;

const MP3_RATES: [u32; 14] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const OPUS_RATE: u32 = 48_000;
/// 20 ms.
const OPUS_FRAME: usize = 960;
/// Opus at this rate is transparent to nearly everybody; there is no call to
/// spend more when nothing asked for less.
const OPUS_DEFAULT_KBPS: u32 = 192;
const MP3_DEFAULT_KBPS: u32 = 320;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    Mp3,
    Opus,
    Wav,
}

impl Codec {
    fn parse(name: &str) -> Option<Codec> {
        match name.to_ascii_lowercase().as_str() {
            "mp3" => Some(Codec::Mp3),
            "opus" | "ogg" | "oga" => Some(Codec::Opus),
            "wav" | "pcm" => Some(Codec::Wav),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Codec::Mp3 => "mp3",
            Codec::Opus => "opus",
            Codec::Wav => "pcm",
        }
    }

    fn container(self) -> &'static str {
        match self {
            Codec::Mp3 => "mp3",
            Codec::Opus => "ogg",
            Codec::Wav => "wav",
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Codec::Mp3 => "audio/mpeg",
            Codec::Opus => "audio/ogg",
            Codec::Wav => "audio/wav",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Target {
    codec: Codec,
    /// Kbit/s; ignored for WAV.
    kbps: u32,
    channels: u16,
    offset_s: u64,
}

impl From<TranscodeFormat> for Codec {
    fn from(format: TranscodeFormat) -> Codec {
        match format {
            TranscodeFormat::Mp3 => Codec::Mp3,
            TranscodeFormat::Opus => Codec::Opus,
            TranscodeFormat::Wav => Codec::Wav,
        }
    }
}

async fn probe(http: &Http, path: &Path) -> Option<Probe> {
    let len = tokio::fs::metadata(path).await.ok()?.len();
    if let Some((cached_len, p)) = http.probes.lock().unwrap().get(&path.to_path_buf()) {
        if *cached_len == len {
            return Some(p.clone());
        }
    }
    let owned = path.to_path_buf();
    let p = tokio::task::spawn_blocking(move || audio::probe(&owned))
        .await
        .ok()?;
    http.probes
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (len, p.clone()));
    Some(p)
}

/// `stream`'s transcoding half: `Some` when the file is not to be sent as it is. It is sent
/// as it is when `raw` is asked for, when the host has no `transcoding`, or when it already
/// meets what was asked: the named format, the bit-rate limit (the request's or the
/// account's) and no offset. A backend can also ask for a transcode (`Media::Transcode`).
pub(super) async fn stream(
    http: &Http,
    p: &Params,
    who: &Who,
    media: &Source,
    head: bool,
) -> Option<Response<Body>> {
    let format = p.get("format").filter(|f| !f.is_empty());
    let offset_s = p.int("timeOffset").unwrap_or(0).max(0) as u64;
    let target = if let Some((format, offset_ms)) = media.forced {
        let codec = Codec::from(format);
        let probed = probe(http, &media.path).await;
        let default = if codec == Codec::Opus {
            OPUS_DEFAULT_KBPS
        } else {
            MP3_DEFAULT_KBPS
        };
        Target {
            codec,
            kbps: default,
            channels: probed.map_or(2, |p| p.channels.clamp(1, 2)),
            offset_s: offset_ms / 1000,
        }
    } else if format == Some("raw") || !http.srv.backend.capabilities().transcoding {
        return None;
    } else {
        let requested = p.int("maxBitRate").unwrap_or(0).max(0) as u32;
        let limit = match (requested, who.account.max_bit_rate) {
            (0, account) => account,
            (asked, 0) => asked,
            (asked, account) => asked.min(account),
        };
        let probed = probe(http, &media.path).await;
        choose_target(media, probed.as_ref(), format, limit, offset_s)?
    };
    let estimate = p.truthy("estimateContentLength").unwrap_or(false);
    Some(send_transcoded(media, target, estimate, head))
}

fn choose_target(
    media: &Source,
    probed: Option<&Probe>,
    format: Option<&str>,
    limit: u32,
    offset_s: u64,
) -> Option<Target> {
    let probed = probed.filter(|p| p.decodable)?;
    let wanted = format.and_then(Codec::parse);
    let suffix = suffix_of(&media.path);
    let same_format = format.is_none_or(|f| f.eq_ignore_ascii_case(&suffix)) || wanted.is_none();
    let over_limit = limit > 0 && media.bitrate > limit;
    if same_format && !over_limit && offset_s == 0 {
        return None;
    }
    // What's left is a limit (lossy, and MP3 plays everywhere) or an offset
    // (nothing lost, and only this network to carry it).
    let codec = wanted.unwrap_or(if over_limit { Codec::Mp3 } else { Codec::Wav });
    let default = if codec == Codec::Opus {
        OPUS_DEFAULT_KBPS
    } else {
        MP3_DEFAULT_KBPS
    };
    let kbps = if limit > 0 {
        limit.min(default)
    } else {
        default
    };
    Some(Target {
        codec,
        kbps,
        channels: probed.channels.clamp(1, 2),
        offset_s,
    })
}

fn send_transcoded(media: &Source, target: Target, estimate: bool, head: bool) -> Response<Body> {
    let mut res = Response::new(empty());
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(target.codec.mime()),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    let left_ms = media.duration_ms.saturating_sub(target.offset_s * 1000);
    if estimate && left_ms > 0 {
        let bytes = match target.codec {
            Codec::Wav => left_ms * 48 * target.channels as u64 * 2 + audio::WAV_HEADER_LEN,
            _ => left_ms * target.kbps as u64 / 8,
        };
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes));
    }
    if head {
        return res;
    }
    let (tx, body) = channel_body(8);
    let path = media.path.clone();
    tokio::task::spawn_blocking(move || {
        let sink = |bytes: Vec<u8>| tx.blocking_send(Ok(Bytes::from(bytes))).is_ok();
        let done = match target.codec {
            Codec::Opus => encode_opus(&path, target, sink),
            Codec::Mp3 => encode_mp3(&path, target, sink),
            Codec::Wav => encode_wav(&path, target, sink),
        };
        if let Err(e) = done {
            let _ = tx.blocking_send(Err(std::io::Error::other(e)));
        }
    });
    *res.body_mut() = body;
    res
}

// ---- Decoding -----------------------------------------------------------------

/// The file from `offset_s` on as blocks of interleaved samples, at
/// `target_rate` (0 keeps the file's) and in `channels`.
fn decode(
    path: &Path,
    offset_s: u64,
    channels: u16,
    target_rate: impl Fn(u32) -> u32,
    mut each: impl FnMut(&[f32], u32) -> bool,
) -> Result<(), String> {
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
        .map_err(|e| e.to_string())?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("no audio track")?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;
    // Containers land on a packet at or before the time; the rest is decoded
    // and dropped, so the first sample out is the right one.
    let mut skip_frames = 0u64;
    if offset_s > 0 {
        let seeked = format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::new(offset_s, 0.0),
                    track_id: Some(track_id),
                },
            )
            .map_err(|e| e.to_string())?;
        skip_frames = seeked.required_ts.saturating_sub(seeked.actual_ts);
        decoder.reset();
    }

    let mut samples: Option<SampleBuffer<f32>> = None;
    let mut resamplers: Vec<Resampler> = Vec::new();
    let mut planes: Vec<Vec<f32>> = Vec::new();
    let mut out: Vec<f32> = Vec::new();
    let out_channels = channels.clamp(1, 2) as usize;
    let mut rate_out = 0u32;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue,
            Err(SymError::IoError(_)) => break,
            Err(e) => return Err(e.to_string()),
        };
        let spec = *decoded.spec();
        let in_channels = spec.channels.count().max(1);
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        let buf = match &mut samples {
            Some(b) if b.capacity() >= frames * in_channels => b,
            _ => {
                samples = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                samples.as_mut().unwrap()
            }
        };
        buf.copy_interleaved_ref(decoded);
        let mut interleaved = buf.samples();
        if skip_frames > 0 {
            let drop = skip_frames.min(frames as u64) as usize;
            skip_frames -= drop as u64;
            interleaved = &interleaved[(drop * in_channels).min(interleaved.len())..];
            if interleaved.is_empty() {
                continue;
            }
        }
        if rate_out == 0 {
            rate_out = target_rate(spec.rate);
            if rate_out == 0 {
                rate_out = spec.rate;
            }
            if rate_out != spec.rate {
                resamplers = (0..out_channels)
                    .map(|_| Resampler::new(spec.rate, rate_out))
                    .collect();
            }
            planes = vec![Vec::new(); out_channels];
        }
        mix_into(interleaved, in_channels, out_channels, &mut planes);
        out.clear();
        interleave(&mut planes, &mut resamplers, &mut out, false);
        if !out.is_empty() && !each(&out, rate_out) {
            return Ok(());
        }
    }
    if rate_out != 0 && !resamplers.is_empty() {
        out.clear();
        interleave(&mut planes, &mut resamplers, &mut out, true);
        if !out.is_empty() {
            each(&out, rate_out);
        }
    }
    Ok(())
}

/// Splits interleaved input into `out` planes: mono to both, anything wider
/// folded down.
fn mix_into(input: &[f32], from: usize, to: usize, planes: &mut [Vec<f32>]) {
    for plane in planes.iter_mut() {
        plane.clear();
    }
    for frame in input.chunks_exact(from) {
        if to == 1 {
            planes[0].push(frame.iter().sum::<f32>() / from as f32);
        } else if from == 1 {
            planes[0].push(frame[0]);
            planes[1].push(frame[0]);
        } else {
            planes[0].push(frame[0]);
            planes[1].push(frame[1]);
        }
    }
}

fn interleave(
    planes: &mut [Vec<f32>],
    resamplers: &mut [Resampler],
    out: &mut Vec<f32>,
    finishing: bool,
) {
    let n = planes.len();
    if resamplers.is_empty() {
        let frames = planes[0].len();
        for i in 0..frames {
            for plane in planes.iter() {
                out.push(plane[i]);
            }
        }
        return;
    }
    let mut converted: Vec<Vec<f32>> = vec![Vec::new(); n];
    for ch in 0..n {
        if finishing {
            resamplers[ch].finish(&mut converted[ch]);
        } else {
            resamplers[ch].push(&planes[ch], &mut converted[ch]);
        }
    }
    let frames = converted.iter().map(Vec::len).min().unwrap_or(0);
    for i in 0..frames {
        for plane in &converted {
            out.push(plane[i]);
        }
    }
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

// ---- WAV ----------------------------------------------------------------------

fn encode_wav(
    path: &Path,
    target: Target,
    mut sink: impl FnMut(Vec<u8>) -> bool,
) -> Result<(), String> {
    let mut started = false;
    decode(
        path,
        target.offset_s,
        target.channels,
        |_| 0,
        |samples, rate| {
            let mut bytes = Vec::with_capacity(samples.len() * 2 + 44);
            if !started {
                started = true;
                bytes.extend_from_slice(&audio::wav_header(rate, target.channels, 16, None));
            }
            for &s in samples {
                bytes.extend_from_slice(&to_i16(s).to_le_bytes());
            }
            sink(bytes)
        },
    )
}

// ---- MP3 ----------------------------------------------------------------------

fn encode_mp3(
    path: &Path,
    target: Target,
    mut sink: impl FnMut(Vec<u8>) -> bool,
) -> Result<(), String> {
    let mut encoder = Mp3Encoder::new(Mp3EncoderConfig {
        bitrate_kbps: target.kbps,
        vbr_quality: None,
    });
    let mut failure: Option<String> = None;
    let mut bytes: Vec<u8> = Vec::new();
    decode(
        path,
        target.offset_s,
        target.channels,
        // The rates Layer III has: anything higher comes down to its family's.
        |rate| match rate {
            0..=48_000 => rate,
            r if r % 44_100 == 0 => 44_100,
            _ => 48_000,
        },
        |samples, rate| {
            if let Err(e) = encoder.push_pcm_f32(samples, target.channels, rate) {
                failure = Some(format!("{e:?}"));
                return false;
            }
            bytes.clear();
            while let Ok(packet) = encoder.next_packet() {
                bytes.extend_from_slice(&packet);
            }
            bytes.is_empty() || sink(std::mem::take(&mut bytes))
        },
    )?;
    if let Some(failure) = failure {
        return Err(failure);
    }
    encoder.finish();
    bytes.clear();
    while let Ok(packet) = encoder.next_packet() {
        bytes.extend_from_slice(&packet);
    }
    if !bytes.is_empty() {
        sink(bytes);
    }
    Ok(())
}

// ---- Opus in Ogg --------------------------------------------------------------

fn encode_opus(
    path: &Path,
    target: Target,
    mut sink: impl FnMut(Vec<u8>) -> bool,
) -> Result<(), String> {
    let channels = target.channels as usize;
    let mut encoder = OpusEncoder::new(
        OPUS_RATE as i32,
        target.channels as usize,
        Application::Audio,
    )
    .map_err(|e| format!("{e:?}"))?;
    encoder.bitrate_bps = (target.kbps * 1000) as i32;
    // The encoder's delay: decoders skip this many samples at the start.
    const PRE_SKIP: u64 = 312;
    let mut ogg = OggWriter::new();
    let mut started = false;
    let mut pending: Vec<f32> = Vec::with_capacity(OPUS_FRAME * channels * 2);
    let mut packet = vec![0u8; 4000];
    let mut granule = PRE_SKIP;
    let mut bytes: Vec<u8> = Vec::new();
    let mut stopped = false;
    let mut failure: Option<String> = None;

    decode(
        path,
        target.offset_s,
        target.channels,
        |_| OPUS_RATE,
        |samples, _| {
            bytes.clear();
            if !started {
                started = true;
                ogg.page(
                    &opus_head(target.channels as u8, PRE_SKIP as u16),
                    0,
                    PageKind::First,
                    &mut bytes,
                );
                ogg.page(&opus_tags(), 0, PageKind::Middle, &mut bytes);
            }
            pending.extend_from_slice(samples);
            let frame_len = OPUS_FRAME * channels;
            let mut used = 0;
            while pending.len() - used >= frame_len {
                match encoder.encode(&pending[used..used + frame_len], OPUS_FRAME, &mut packet) {
                    Ok(len) => {
                        granule += OPUS_FRAME as u64;
                        ogg.page(&packet[..len], granule, PageKind::Middle, &mut bytes);
                    }
                    Err(e) => {
                        failure = Some(format!("{e:?}"));
                        return false;
                    }
                }
                used += frame_len;
            }
            pending.drain(..used);
            if !bytes.is_empty() && !sink(std::mem::take(&mut bytes)) {
                stopped = true;
                return false;
            }
            true
        },
    )?;
    if let Some(failure) = failure {
        return Err(failure);
    }
    if stopped || !started {
        return Ok(());
    }
    // The last frame, padded; the page's granule says how much of it counts.
    let real = pending.len() / channels;
    pending.resize(OPUS_FRAME * channels, 0.0);
    let len = encoder
        .encode(&pending, OPUS_FRAME, &mut packet)
        .map_err(|e| format!("{e:?}"))?;
    granule += real as u64;
    let mut tail = Vec::new();
    ogg.page(&packet[..len], granule, PageKind::Last, &mut tail);
    sink(tail);
    Ok(())
}

fn opus_head(channels: u8, pre_skip: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(channels);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&OPUS_RATE.to_le_bytes());
    head.extend_from_slice(&0u16.to_le_bytes());
    head.push(0);
    head
}

fn opus_tags() -> Vec<u8> {
    let vendor = b"Elbert";
    let mut tags = Vec::with_capacity(20);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());
    tags
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageKind {
    First,
    Middle,
    Last,
}

/// One packet per page: simple, and a few bytes dearer than packing them.
struct OggWriter {
    serial: u32,
    sequence: u32,
}

impl OggWriter {
    fn new() -> Self {
        let mut bytes = [0u8; 4];
        let _ = getrandom::fill(&mut bytes);
        OggWriter {
            serial: u32::from_ne_bytes(bytes),
            sequence: 0,
        }
    }

    fn page(&mut self, packet: &[u8], granule: u64, kind: PageKind, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(b"OggS");
        out.push(0);
        out.push(match kind {
            PageKind::First => 2,
            PageKind::Middle => 0,
            PageKind::Last => 4,
        });
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&self.serial.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        self.sequence += 1;
        let segments = packet.len() / 255 + 1;
        out.push(segments as u8);
        for _ in 0..segments - 1 {
            out.push(255);
        }
        out.push((packet.len() % 255) as u8);
        out.extend_from_slice(packet);
        let crc = ogg_crc(&out[start..]);
        out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
    }
}

fn ogg_crc(data: &[u8]) -> u32 {
    fn table() -> &'static [u32; 256] {
        static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
        TABLE.get_or_init(|| {
            let mut t = [0u32; 256];
            for (i, slot) in t.iter_mut().enumerate() {
                let mut r = (i as u32) << 24;
                for _ in 0..8 {
                    r = if r & 0x8000_0000 != 0 {
                        (r << 1) ^ 0x04C1_1DB7
                    } else {
                        r << 1
                    };
                }
                *slot = r;
            }
            t
        })
    }
    let t = table();
    data.iter().fold(0u32, |crc, &b| {
        (crc << 8) ^ t[((crc >> 24) as u8 ^ b) as usize]
    })
}

// ---- The transcoding extension --------------------------------------------------

fn source_details(media: &Source, probed: &Probe) -> Value {
    let suffix = suffix_of(&media.path);
    let (container, codec) = source_codec(&suffix, probed);
    let mut value = json!({
        "protocol": "http",
        "container": container,
        "codec": codec,
        "audioChannels": probed.channels,
        "audioSamplerate": probed.sample_rate,
        "audioBitdepth": probed.bits,
    });
    if media.bitrate > 0 {
        value["audioBitrate"] = (media.bitrate * 1000).into();
    }
    value
}

/// What the file is, in the extension's words.
fn source_codec(suffix: &str, probed: &Probe) -> (String, String) {
    let container = match suffix {
        "m4a" | "m4b" | "alac" | "aac" | "mp4" => "mp4",
        other => other,
    }
    .to_string();
    let codec = match (suffix, probed.delivery) {
        ("m4a" | "m4b" | "alac" | "mp4", Delivery::Wav) => "alac",
        ("m4a" | "m4b" | "aac" | "mp4", _) => "aac",
        ("ogg" | "oga", _) => "vorbis",
        ("wav" | "aif" | "aiff", _) => "pcm",
        (other, _) => other,
    }
    .to_string();
    (container, codec)
}

fn listed(list: &Value, value: &str) -> bool {
    match list.as_array() {
        None => true,
        Some(items) => {
            items.is_empty()
                || items
                    .iter()
                    .any(|i| i.as_str().is_some_and(|s| s.eq_ignore_ascii_case(value)))
        }
    }
}

/// `true` when `limit` holds for `actual` (a required one that can't be known
/// doesn't count against the file).
fn limit_holds(limit: &Value, actual: Option<i64>) -> bool {
    let Some(actual) = actual else { return true };
    let values = limit["values"].as_str().map(str::to_string).or_else(|| {
        limit["values"].as_array().map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
    });
    let Some(values) = values else { return true };
    let numbers: Vec<i64> = values
        .split(['|', ','])
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    match limit["comparison"].as_str().unwrap_or("") {
        "Equals" => numbers.is_empty() || numbers.contains(&actual),
        "NotEquals" => !numbers.contains(&actual),
        "LessThanEqual" => numbers.first().is_none_or(|n| actual <= *n),
        "GreaterThanEqual" => numbers.first().is_none_or(|n| actual >= *n),
        _ => true,
    }
}

fn codec_limits_hold(
    client: &Value,
    codec: &str,
    media: &Source,
    probed: &Probe,
    reasons: &mut Vec<String>,
) -> bool {
    let mut fine = true;
    for profile in client["codecProfiles"].as_array().into_iter().flatten() {
        if !profile["name"]
            .as_str()
            .is_some_and(|n| n.eq_ignore_ascii_case(codec))
        {
            continue;
        }
        for limit in profile["limitations"].as_array().into_iter().flatten() {
            let name = limit["name"].as_str().unwrap_or("");
            let actual = match name {
                "audioChannels" => Some(probed.channels as i64),
                "audioBitrate" => (media.bitrate > 0).then_some(media.bitrate as i64 * 1000),
                "audioSamplerate" => Some(probed.sample_rate as i64),
                "audioBitdepth" => Some(probed.bits as i64),
                _ => None,
            };
            if !limit_holds(limit, actual) && limit["required"].as_bool().unwrap_or(false) {
                fine = false;
                reasons.push(format!("{name} is not supported"));
            }
        }
    }
    fine
}

pub async fn decision(
    http: &Http,
    p: &Params,
    who: &Who,
    body: Option<&[u8]>,
    is_post: bool,
) -> Answer {
    who.require(who.account.stream, "Streaming")?;
    if !is_post {
        return Err(ApiError(ERR_GENERIC, "This call takes a POST".into()));
    }
    let id = p.need("mediaId")?;
    match p.need("mediaType")? {
        "song" | "podcast" => {}
        other => {
            return Err(ApiError(
                ERR_GENERIC,
                format!("Unknown media type: {other}"),
            ))
        }
    }
    let client: Value = body
        .and_then(|b| serde_json::from_slice(b).ok())
        .ok_or_else(|| {
            ApiError(
                ERR_GENERIC,
                "The request body must be a JSON ClientInfo".into(),
            )
        })?;
    let media = find_file(&http.srv, id, who).await?;
    let probed = probe(http, &media.path)
        .await
        .ok_or_else(|| ApiError::not_found("File"))?;
    let suffix = suffix_of(&media.path);
    let (container, codec) = source_codec(&suffix, &probed);

    let mut reasons: Vec<String> = Vec::new();
    let max_direct = client["maxAudioBitrate"].as_i64().unwrap_or(0);
    let bitrate_bps = media.bitrate as i64 * 1000;
    let mut direct = false;
    for profile in client["directPlayProfiles"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let http = listed(&profile["protocols"], "http");
        let channels = profile["maxAudioChannels"]
            .as_i64()
            .is_none_or(|m| probed.channels as i64 <= m);
        if http
            && channels
            && listed(&profile["containers"], &container)
            && listed(&profile["audioCodecs"], &codec)
        {
            direct = true;
            break;
        }
    }
    if !direct {
        reasons.push("container or codec is not supported".into());
    }
    if max_direct > 0 && bitrate_bps > max_direct {
        direct = false;
        reasons.push("audio bitrate is too high".into());
    }
    if !codec_limits_hold(&client, &codec, &media, &probed, &mut reasons) {
        direct = false;
    }
    if who.account.max_bit_rate > 0 && media.bitrate > who.account.max_bit_rate {
        direct = false;
        reasons.push("audio bitrate is over this account's limit".into());
    }

    let mut out = json!({
        "canDirectPlay": direct,
        "canTranscode": false,
        "sourceStream": source_details(&media, &probed),
    });
    if !direct {
        out["transcodeReason"] = reasons.clone().into();
        if !probed.decodable {
            out["errorReason"] = "This format can't be decoded here".into();
            return Ok(json!({ "transcodeDecision": out }));
        }
        let transcode_cap = client["maxTranscodingAudioBitrate"]
            .as_i64()
            .filter(|&b| b > 0)
            .unwrap_or(i64::MAX);
        let account_cap = if who.account.max_bit_rate > 0 {
            who.account.max_bit_rate as i64 * 1000
        } else {
            i64::MAX
        };
        for profile in client["transcodingProfiles"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if profile["protocol"].as_str() != Some("http") {
                continue;
            }
            let chosen = profile["audioCodec"]
                .as_str()
                .and_then(Codec::parse)
                .or_else(|| profile["container"].as_str().and_then(Codec::parse));
            let Some(chosen) = chosen else { continue };
            let channels = profile["maxAudioChannels"]
                .as_i64()
                .unwrap_or(2)
                .clamp(1, 2)
                .min(probed.channels as i64) as u16;
            let default = if chosen == Codec::Opus {
                OPUS_DEFAULT_KBPS
            } else {
                MP3_DEFAULT_KBPS
            };
            let kbps = (transcode_cap.min(account_cap) / 1000).clamp(32, default as i64) as u32;
            let kbps = if chosen == Codec::Mp3 {
                MP3_RATES
                    .iter()
                    .rev()
                    .find(|&&r| r <= kbps)
                    .copied()
                    .unwrap_or(32)
            } else {
                kbps
            };
            let rate = match chosen {
                Codec::Opus => OPUS_RATE,
                Codec::Mp3 if !matches!(probed.sample_rate, 44_100 | 48_000 | 32_000) => 44_100,
                _ => probed.sample_rate,
            };
            let mut details = json!({
                "protocol": "http",
                "container": chosen.container(),
                "codec": chosen.name(),
                "audioChannels": channels,
                "audioSamplerate": rate,
            });
            if chosen != Codec::Wav {
                details["audioBitrate"] = (kbps * 1000).into();
            }
            out["canTranscode"] = true.into();
            out["transcodeParams"] = format!("{}.{kbps}.{channels}", chosen.container()).into();
            out["transcodeStream"] = details;
            return Ok(json!({ "transcodeDecision": out }));
        }
        out["errorReason"] =
            "None of the client's transcoding profiles can be produced here".into();
    }
    Ok(json!({ "transcodeDecision": out }))
}

/// `getTranscodeStream`: `transcodeParams` is only ever what
/// `decision` gave out: `container.kbps.channels`.
pub async fn transcode_stream(
    srv: &Arc<Server>,
    req: &Request<()>,
    p: &Params,
    who: &Who,
    head: bool,
) -> Result<Response<Body>, ApiError> {
    who.require(who.account.stream, "Streaming")?;
    let id = p.need("mediaId")?;
    p.need("mediaType")?;
    let params = p.need("transcodeParams")?;
    let bad = || {
        ApiError(
            ERR_GENERIC,
            "Those are not parameters this server gave out".into(),
        )
    };
    let mut parts = params.split('.');
    let codec = parts.next().and_then(Codec::parse).ok_or_else(bad)?;
    let kbps: u32 = parts.next().and_then(|k| k.parse().ok()).ok_or_else(bad)?;
    let channels: u16 = parts
        .next()
        .and_then(|c| c.parse().ok())
        .filter(|&c| c == 1 || c == 2)
        .ok_or_else(bad)?;
    let offset_s = p.int("offset").unwrap_or(0).max(0) as u64;
    let media = find_file(srv, id, who).await?;
    let target = Target {
        codec,
        kbps: kbps.clamp(32, 510),
        channels,
        offset_s,
    };
    if tokio::fs::metadata(&media.path).await.is_err() {
        return Err(ApiError::not_found("File"));
    }
    let _ = (srv, req);
    Ok(send_transcoded(&media, target, false, head))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn probe_of(channels: u16, rate: u32) -> Probe {
        Probe {
            delivery: Delivery::Wav,
            decodable: true,
            sample_rate: rate,
            channels,
            bits: 16,
        }
    }

    fn media(path: &str, bitrate: u32) -> Source {
        Source {
            path: PathBuf::from(path),
            mime: "audio/mpeg".into(),
            duration_ms: 60_000,
            bitrate,
            forced: None,
        }
    }

    #[test]
    fn the_file_is_sent_unless_something_asks_otherwise() {
        let p = probe_of(2, 44_100);
        let flac = media("/a.flac", 900);
        assert_eq!(choose_target(&flac, Some(&p), None, 0, 0), None);
        assert_eq!(choose_target(&flac, Some(&p), Some("flac"), 0, 0), None);
        // A format nothing here can make isn't a reason to refuse.
        assert_eq!(choose_target(&flac, Some(&p), Some("aac"), 0, 0), None);
        // Under the limit, it stands.
        assert_eq!(
            choose_target(&media("/a.mp3", 128), Some(&p), None, 320, 0),
            None
        );

        let t = choose_target(&flac, Some(&p), None, 192, 0).unwrap();
        assert_eq!((t.codec, t.kbps), (Codec::Mp3, 192));
        let t = choose_target(&flac, Some(&p), Some("opus"), 0, 0).unwrap();
        assert_eq!((t.codec, t.kbps), (Codec::Opus, OPUS_DEFAULT_KBPS));
        let t = choose_target(&flac, Some(&p), None, 0, 30).unwrap();
        assert_eq!((t.codec, t.offset_s), (Codec::Wav, 30));
        // What can't be decoded goes as it is.
        assert_eq!(choose_target(&flac, None, Some("mp3"), 0, 0), None);
    }

    #[test]
    fn limits() {
        let limit = |c: &str, v: &str| json!({ "name": "audioChannels", "comparison": c, "values": v, "required": true });
        assert!(limit_holds(&limit("LessThanEqual", "2"), Some(2)));
        assert!(!limit_holds(&limit("LessThanEqual", "2"), Some(6)));
        assert!(limit_holds(&limit("GreaterThanEqual", "2"), Some(2)));
        assert!(!limit_holds(&limit("Equals", "1"), Some(2)));
        assert!(!limit_holds(&limit("NotEquals", "2"), Some(2)));
        assert!(limit_holds(&limit("Equals", "1"), None));
    }

    #[test]
    fn ogg_crc_matches_the_spec() {
        // The checksum of "OggS" alone, by the polynomial RFC 3533 names.
        assert_eq!(ogg_crc(b"OggS"), 0x5fb0_a94f);
    }
}
