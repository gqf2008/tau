//! Host-side audio I/O for realtime-av Phase 0 (docs/realtime-av.md).
//!
//! The architecture red line keeps capture/playback in the host (WASI
//! has no audio devices; a wasm guest never touches a mic). Consent for
//! THIS caller is the explicit `/mic` command — the red line's consent
//! category governs wasm guests; the host CLI acting on the user's
//! command holds the same authority as the user's keyboard.
//!
//! Everything here speaks one container: 16-bit PCM WAV (self-
//! describing — rate/depth travel in the header, so the downlink
//! assembly of same-media_type deltas needs zero parsing).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Record `seconds` from the default input device and return a WAV
/// container. `sine` instead synthesizes a 440 Hz tone — the
/// hardware-free, deterministic gate path (validate.sh asserts on it;
/// a silent mic proves nothing).
pub fn record_wav(seconds: u32, sine: bool) -> Result<Vec<u8>> {
    if sine {
        return Ok(sine_wav(seconds));
    }
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default input device (microphone)")?;
    let supported = device
        .default_input_config()
        .context("input device has no default config")?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels as usize;
    let sample_rate = config.sample_rate;

    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let err = |e| eprintln!("[tau] mic stream: {e}");
    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let tx = tx.clone();
            device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    let _ = tx.send(data.to_vec());
                },
                err,
                None,
            )?
        }
        cpal::SampleFormat::I16 => {
            let tx = tx.clone();
            device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let _ = tx
                        .send(data.iter().map(|s| *s as f32 / 32768.0).collect());
                },
                err,
                None,
            )?
        }
        other => bail!("unsupported mic sample format: {other}"),
    };
    stream.play()?;
    std::thread::sleep(Duration::from_secs(u64::from(seconds)));
    drop(stream);

    let mut samples = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        samples.extend(chunk);
    }
    // Downmix to mono (the provider-facing shape; multichannel audio
    // is not a Phase 0 concern).
    let mono: Vec<i16> = samples
        .chunks(channels.max(1))
        .map(|frame| {
            let mean = frame.iter().sum::<f32>() / frame.len() as f32;
            (mean * 32767.0).clamp(-32768.0, 32767.0) as i16
        })
        .collect();
    if mono.is_empty() {
        bail!("the mic produced zero samples in {seconds}s");
    }
    Ok(wav_encode(&mono, sample_rate))
}

/// Play a WAV through the default output device; returns the samples
/// played. A missing output device or unsupported config is an Err the
/// caller reports as a NOTICE — headless machines must not go red.
pub fn play_wav(wav: &[u8]) -> Result<u64> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(wav))
        .context("not a readable WAV")?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        bail!("unsupported WAV shape ({} bit {:?})", spec.bits_per_sample, spec.sample_format);
    }
    let mut samples: Vec<f32> = Vec::new();
    for ch in reader.samples::<i16>().step_by(1) {
        samples.push(ch? as f32 / 32768.0);
    }
    // Downmix multichannel WAVs the same way capture does.
    if spec.channels > 1 {
        let ch = spec.channels as usize;
        samples = samples
            .chunks(ch)
            .map(|f| f.iter().sum::<f32>() / f.len() as f32)
            .collect();
    }
    let total = samples.len() as u64;
    if total == 0 {
        bail!("the clip is empty");
    }

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default output device (speaker)")?;
    let supported = device
        .default_output_config()
        .context("output device has no default config")?;
    let out_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let out_rate = config.sample_rate;
    let out_channels = config.channels.max(1) as usize;

    // Naive linear resample when the device rate differs from the
    // clip's — Phase 0 playback, not an audio engine.
    let samples = if out_rate != spec.sample_rate {
        resample_linear(&samples, spec.sample_rate, out_rate)
    } else {
        samples
    };

    let play_len = samples.len();
    let pos = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let write_pos = pos.clone();
    let err = |e| eprintln!("[tau] playback stream: {e}");
    let make_cb = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let mut at = write_pos.load(std::sync::atomic::Ordering::Relaxed);
        for frame in data.chunks_mut(out_channels) {
            let s = samples.get(at).copied().unwrap_or(0.0);
            for channel in frame.iter_mut() {
                *channel = s;
            }
            at += 1;
        }
        write_pos.store(at, std::sync::atomic::Ordering::Relaxed);
    };
    let stream = match out_format {
        cpal::SampleFormat::F32 => {
            device.build_output_stream(&config, make_cb, err, None)?
        }
        cpal::SampleFormat::I16 => {
            device.build_output_stream(
                &config,
                move |data: &mut [i16], info: &cpal::OutputCallbackInfo| {
                    // Reuse the f32 callback through a scratch buffer.
                    let mut scratch = vec![0.0f32; data.len()];
                    make_cb(&mut scratch, info);
                    for (dst, src) in data.iter_mut().zip(scratch) {
                        *dst = (src * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    }
                },
                err,
                None,
            )?
        }
        other => bail!("unsupported output sample format: {other}"),
    };
    stream.play()?;

    // Block until the clip has fully played (plus drain margin), with
    // a hard cap so a stalled device cannot hang the REPL.
    let deadline = Instant::now()
        + Duration::from_secs_f32(play_len as f32 / out_rate as f32)
        + Duration::from_secs(2);
    while pos.load(std::sync::atomic::Ordering::Relaxed) < play_len
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(total)
}

/// Stream the default input device as 50ms chunks of 16-bit mono PCM
/// at 16kHz (1600 bytes per chunk — the realtime uplink convention),
/// for `seconds`. Used by the `/live` full-duplex loop; the sine path
/// of that loop needs no device at all.
pub fn stream_mic(seconds: u32) -> Result<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>> {
    const RATE: usize = 16_000;
    const CHUNK: usize = RATE / 20; // 50ms

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default input device (microphone)")?;
    let supported = device
        .default_input_config()
        .context("input device has no default config")?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels.max(1) as usize;
    let in_rate = config.sample_rate;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let pending: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let push = move |data: &[f32]| {
        // Downmix to mono, then chunk at the OUTPUT rate (resample
        // chunk-wise when the device rate differs — Phase 2a capture,
        // not an audio engine).
        let mono: Vec<f32> = data
            .chunks(channels)
            .map(|f| f.iter().sum::<f32>() / f.len() as f32)
            .collect();
        let mono = if in_rate != RATE as u32 {
            resample_linear(&mono, in_rate, RATE as u32)
        } else {
            mono
        };
        let mut pending = pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.extend(mono);
        while pending.len() >= CHUNK {
            let chunk: Vec<f32> = pending.drain(..CHUNK).collect();
            let mut bytes = Vec::with_capacity(CHUNK * 2);
            for s in chunk {
                let v = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            if tx.send(bytes).is_err() {
                return; // the session ended; stop forwarding
            }
        }
    };

    let err = |e| eprintln!("[tau] mic stream: {e}");
    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| push(data),
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                let f: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                push(&f);
            },
            err,
            None,
        )?,
        other => bail!("unsupported mic sample format: {other}"),
    };
    stream.play()?;

    // Time-boxed: dropping the stream after `seconds` ends capture;
    // the receiver closes once the forwarding callback's sender drops.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(u64::from(seconds)));
        drop(stream);
    });
    Ok(rx)
}

// ---------- Phase 1: the live playback sink (docs/realtime-av.md) ----------

/// How many seconds of audio the ring holds; beyond it the OLDEST
/// samples drop — under realtime semantics a backlog is sound the
/// listener can no longer catch up to.
const RING_SECONDS: u32 = 4;

/// Live downlink sink: `AudioDelta` chunks in, speaker out. One per
/// renderer (interactive + print). The output stream opens lazily on
/// the first chunk — text-only sessions never touch audio hardware —
/// and a missing/unusable device degrades to a NULL sink that still
/// decodes and counts every sample, so the gate asserts "it streamed"
/// on machines with no speaker.
pub struct PlaybackSink {
    output: Option<Output>,
    /// Incremental decoder for the current segment (same-media_type
    /// chunks of one container; a media_type switch clears it).
    decoder: Option<Decoder>,
    /// Samples pushed this run (input rate, pre-resample) — the
    /// assertable counter, shared with the post-run replay gate.
    streamed: Arc<AtomicU64>,
    /// A new segment started and the renderer has not announced it.
    pending_announce: bool,
    /// The null-sink notice went out once.
    noticed_null: bool,
}

struct Output {
    _stream: cpal::Stream,
    ring: Arc<Mutex<VecDeque<f32>>>,
    rate: u32,
}

struct Decoder {
    media_type: String,
    rate: u32,
    channels: usize,
    /// WAV: unparsed header bytes accumulate here until the RIFF walk
    /// finds the data chunk; empty afterwards.
    header: Vec<u8>,
    header_done: bool,
    /// One carried byte when a chunk split a 16-bit sample.
    tail: Option<u8>,
    /// A megabyte without a data chunk is not a WAV — fail open:
    /// count nothing, play nothing, stay silent.
    broken: bool,
}

impl PlaybackSink {
    pub fn new() -> Self {
        Self {
            output: None,
            decoder: None,
            streamed: Arc::new(AtomicU64::new(0)),
            pending_announce: false,
            noticed_null: false,
        }
    }

    /// Shared counter for the post-run replay gate (assembled blocks
    /// whose deltas already played live must not replay).
    pub fn streamed_handle(&self) -> Arc<AtomicU64> {
        self.streamed.clone()
    }

    /// Samples pushed since the last `begin_run`/`clear`.
    pub fn streamed(&self) -> u64 {
        self.streamed.load(Ordering::Relaxed)
    }

    /// "{media_type} @ {rate}kHz" once the rate is known, else the bare
    /// media type (a WAV header still accumulating).
    pub fn desc(&self) -> String {
        match &self.decoder {
            Some(d) if d.header_done && !d.broken => {
                format!("{} @ {}", d.media_type, format_rate(d.rate))
            }
            Some(d) => d.media_type.clone(),
            None => "no stream".to_string(),
        }
    }

    /// New run: reset the per-run counter (the decoder survives — a
    /// segment spanning the boundary is not a real shape, but the next
    /// chunk's media_type check would rebuild it anyway).
    pub fn begin_run(&mut self) {
        self.streamed.store(0, Ordering::Relaxed);
        self.pending_announce = false;
    }

    /// Feed one AudioDelta chunk. Returns true when a NEW segment just
    /// started (the renderer announces it with `desc()`).
    pub fn push(&mut self, data: &[u8], media_type: &str) -> bool {
        if self.decoder.as_ref().is_some_and(|d| d.media_type != media_type) {
            // Segment switch: stale sound must not leak into the next
            // segment — same rule as an interrupt.
            self.clear();
        }
        let started = self.decoder.is_none();
        if started {
            self.decoder = Some(Decoder::new(media_type));
            self.pending_announce = true;
        }
        let samples = match self.decoder.as_mut() {
            Some(d) => d.feed(data),
            None => return false,
        };
        if samples.is_empty() {
            return started;
        }
        self.streamed.fetch_add(samples.len() as u64, Ordering::Relaxed);
        self.ensure_output();
        if let Some(out) = &self.output {
            // Chunk-wise naive resample (boundary clicks are accepted —
            // Phase 1 playback, not an audio engine); the null sink
            // skips it but still counted above.
            let decoder = self.decoder.as_ref().expect("decoder alive");
            let pcm = if out.rate != decoder.rate {
                resample_linear(&samples, decoder.rate, out.rate)
            } else {
                samples
            };
            let mut ring = out.ring.lock().unwrap_or_else(|p| p.into_inner());
            let cap = RING_SECONDS as usize * out.rate as usize;
            ring.extend(pcm);
            while ring.len() > cap {
                ring.pop_front();
            }
        }
        started
    }

    /// Renderer announces once per segment; clears the pending flag.
    pub fn take_announce(&mut self) -> bool {
        std::mem::take(&mut self.pending_announce)
    }

    /// Interrupt / abort: drop every buffered and half-parsed sample.
    /// Silence NOW beats the tail of a canceled answer.
    pub fn clear(&mut self) {
        if let Some(out) = &self.output {
            out.ring
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
        }
        self.decoder = None;
        self.streamed.store(0, Ordering::Relaxed);
        self.pending_announce = false;
    }

    /// Run finished: the summary line for the renderer, if anything
    /// streamed. Does NOT reset the counter — the post-run replay gate
    /// reads it between RunEnd and the next RunStart.
    pub fn end_run(&mut self) -> Option<String> {
        self.pending_announce = false;
        let n = self.streamed();
        (n > 0).then(|| format!("[tau] ▶ streamed {n} samples ({})", self.desc()))
    }

    fn ensure_output(&mut self) {
        if self.output.is_some() {
            return;
        }
        match Output::open() {
            Ok(out) => self.output = Some(out),
            Err(e) => {
                if !self.noticed_null {
                    eprintln!("[tau] playback: {e:#} — null sink (counting silently)");
                    self.noticed_null = true;
                }
            }
        }
    }
}

impl Output {
    fn open() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no default output device (speaker)")?;
        let supported = device
            .default_output_config()
            .context("output device has no default config")?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let rate = config.sample_rate;
        let channels = config.channels.max(1) as usize;

        let ring: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let read_ring = ring.clone();
        let drain = move |data: &mut [f32]| {
            let mut ring = read_ring.lock().unwrap_or_else(|p| p.into_inner());
            for frame in data.chunks_mut(channels) {
                let sample = ring.pop_front().unwrap_or(0.0);
                for channel in frame.iter_mut() {
                    *channel = sample;
                }
            }
        };
        let err = |e| eprintln!("[tau] playback stream: {e}");
        let stream = match format {
            cpal::SampleFormat::F32 => {
                device.build_output_stream(&config, move |d: &mut [f32], _| drain(d), err, None)?
            }
            cpal::SampleFormat::I16 => device.build_output_stream(
                &config,
                move |d: &mut [i16], _| {
                    let mut scratch = vec![0.0f32; d.len()];
                    drain(&mut scratch);
                    for (dst, src) in d.iter_mut().zip(scratch) {
                        *dst = (src * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    }
                },
                err,
                None,
            )?,
            other => bail!("unsupported output sample format: {other}"),
        };
        stream.play()?;
        Ok(Self { _stream: stream, ring, rate })
    }
}

impl Decoder {
    fn new(media_type: &str) -> Self {
        let lower = media_type.to_lowercase();
        // audio/pcm and audio/L16 carry their rate as a MIME parameter
        // (default 24000 — the OpenAI realtime convention), 16-bit LE
        // mono, no container.
        let bare = lower.starts_with("audio/pcm") || lower.starts_with("audio/l16");
        let rate = if bare { mime_rate(&lower).unwrap_or(24_000) } else { 0 };
        Self {
            media_type: media_type.to_string(),
            rate,
            channels: 1,
            header: Vec::new(),
            header_done: bare,
            tail: None,
            broken: false,
        }
    }

    /// Feed a chunk; returns the decoded mono f32 samples (empty while
    /// a WAV header is still accumulating).
    fn feed(&mut self, data: &[u8]) -> Vec<f32> {
        if self.broken {
            return Vec::new();
        }
        if !self.header_done {
            self.header.extend_from_slice(data);
            return match wav_header(&self.header) {
                Some((rate, channels, start)) => {
                    self.rate = rate;
                    self.channels = channels;
                    self.header_done = true;
                    // Split borrows: take the header out of self, then
                    // feed its PCM tail through the normal path.
                    let header = std::mem::take(&mut self.header);
                    self.pcm_bytes(&header[start..])
                }
                None => {
                    if self.header.len() > 1 << 20 {
                        eprintln!("[tau] playback: WAV header never parsed — dropping segment");
                        self.broken = true;
                    }
                    Vec::new()
                }
            };
        }
        self.pcm_bytes(data)
    }

    fn pcm_bytes(&mut self, bytes: &[u8]) -> Vec<f32> {
        let mut raw: Vec<u8> = Vec::with_capacity(bytes.len() + 1);
        if let Some(tail) = self.tail.take() {
            raw.push(tail);
        }
        raw.extend_from_slice(bytes);
        if raw.len() % 2 == 1 {
            self.tail = raw.pop();
        }
        let mut samples: Vec<i16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();
        // Downmix like capture does (Phase 1 clips are mono, but an
        // interleaved stereo WAV must not play at double speed).
        if self.channels > 1 {
            let ch = self.channels;
            samples = samples
                .chunks(ch)
                .map(|frame| frame.iter().map(|&s| s as i32).sum::<i32>() / ch as i32)
                .map(|mean| mean.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                .collect();
        }
        samples.iter().map(|&s| s as f32 / 32768.0).collect()
    }
}

/// Minimal RIFF walk: locate fmt (rate, channels) and the data chunk's
/// start. None means "incomplete" — the caller accumulates more bytes.
/// Only called for media_type audio/wav, so a malformed container just
/// never parses (fail-open notice after 1 MiB).
fn wav_header(bytes: &[u8]) -> Option<(u32, usize, usize)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut at = 12usize;
    let mut fmt: Option<(u32, usize)> = None;
    while at + 8 <= bytes.len() {
        let tag = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        if tag == b"fmt " && at + 16 <= bytes.len() {
            let channels = u16::from_le_bytes(bytes[at + 10..at + 12].try_into().ok()?) as usize;
            let rate = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().ok()?);
            fmt = Some((rate, channels.max(1)));
        }
        if tag == b"data" {
            let (rate, channels) = fmt?;
            return Some((rate, channels, at + 8));
        }
        at += 8 + size + (size & 1);
    }
    None
}

/// `rate=` parameter of a MIME type ("audio/pcm;rate=16000").
fn mime_rate(media_type: &str) -> Option<u32> {
    for part in media_type.split(';').skip(1) {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("rate=") {
            return value.parse().ok();
        }
    }
    None
}

fn format_rate(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{}kHz", rate / 1000)
    } else {
        format!("{:.1}kHz", rate as f64 / 1000.0)
    }
}

/// 440 Hz sine, 16 kHz mono — deterministic, hardware-free.
fn sine_wav(seconds: u32) -> Vec<u8> {
    let rate = 16_000u32;
    let n = (rate * seconds) as usize;
    let samples: Vec<i16> = (0..n)
        .map(|i| {
            let t = i as f32 / rate as f32;
            (440.0 * 2.0 * std::f32::consts::PI * t).sin() * 0.5 * 32767.0
        })
        .map(|s| s as i16)
        .collect();
    wav_encode(&samples, rate)
}

fn wav_encode(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::new(&mut cursor, spec)
            .expect("wav writer over a Vec cannot fail");
        for &sample in samples {
            writer.write_sample(sample).expect("wav write to a Vec cannot fail");
        }
        writer.finalize().expect("wav finalize to a Vec cannot fail");
    }
    cursor.into_inner()
}

fn resample_linear(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    let out_len = samples.len() as u64 * u64::from(to) / u64::from(from);
    (0..out_len as usize)
        .map(|i| {
            let src = i as f64 * f64::from(from) / f64::from(to);
            let idx = src as usize;
            let frac = (src - idx as f64) as f32;
            let a = samples[idx.min(samples.len() - 1)];
            let b = samples[(idx + 1).min(samples.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_wav_round_trips_through_the_player_decode_path() {
        let wav = record_wav(1, true).unwrap();
        // The exact decode half of play_wav, without a device.
        let mut reader = hound::WavReader::new(std::io::Cursor::new(&wav)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.channels, 1);
        let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), 16_000);
        // A 440 Hz sine at amplitude 0.5 must actually contain signal
        // (a silent "sine" would pass every structural check).
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 10_000, "sine peak too low: {peak}");
    }

    #[test]
    fn sink_decodes_wav_chunks_and_counts_every_sample() {
        // The demo echo's shape: one WAV split into three chunks.
        let wav = record_wav(2, true).unwrap();
        let third = wav.len() / 3;
        let mut sink = PlaybackSink::new();
        sink.begin_run();
        assert!(sink.push(&wav[..third], "audio/wav"));
        assert!(sink.take_announce());
        assert!(!sink.push(&wav[third..2 * third], "audio/wav"));
        assert!(!sink.push(&wav[2 * third..], "audio/wav"));
        // Every sample of the 2s @ 16kHz clip decoded and counted —
        // headless (null sink) or not, the counter is the gate.
        assert_eq!(sink.streamed(), 32_000);
        assert_eq!(sink.desc(), "audio/wav @ 16kHz");
        let summary = sink.end_run().unwrap();
        assert!(summary.contains("streamed 32000 samples (audio/wav @ 16kHz)"), "{summary}");
    }

    #[test]
    fn clear_silences_and_zeroes() {
        let wav = record_wav(1, true).unwrap();
        let mut sink = PlaybackSink::new();
        sink.begin_run();
        sink.push(&wav, "audio/wav");
        assert!(sink.streamed() > 0);
        sink.clear();
        assert_eq!(sink.streamed(), 0);
        assert!(sink.end_run().is_none());
    }

    #[test]
    fn media_type_switch_drops_the_stale_segment() {
        let wav = record_wav(1, true).unwrap();
        let mut sink = PlaybackSink::new();
        sink.begin_run();
        sink.push(&wav, "audio/wav");
        assert!(sink.streamed() > 0);
        // A pcm segment starts: the half-parsed WAV must not leak.
        assert!(sink.push(&[0u8; 100], "audio/pcm;rate=24000"));
        assert_eq!(sink.streamed(), 50); // 100 bytes of 16-bit pcm
        assert_eq!(sink.desc(), "audio/pcm;rate=24000 @ 24kHz");
    }

    #[test]
    fn pcm_rate_defaults_to_realtime_convention() {
        let mut sink = PlaybackSink::new();
        sink.begin_run();
        sink.push(&[0u8; 20], "audio/pcm");
        assert_eq!(sink.desc(), "audio/pcm @ 24kHz");
    }

    #[test]
    fn wav_header_walks_real_riff_layout() {
        let wav = record_wav(1, true).unwrap();
        let (rate, channels, start) = wav_header(&wav).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(channels, 1);
        assert!(start >= 44);
        assert_eq!(wav.len() - start, 32_000); // 16k samples * 2 bytes
        // A truncated header reports incomplete, never garbage.
        assert!(wav_header(&wav[..20]).is_none());
    }

    #[test]
    fn resample_preserves_length_ratio() {
        let src = vec![0.5f32; 1000];
        let out = resample_linear(&src, 16_000, 48_000);
        assert_eq!(out.len(), 3000);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }
}
