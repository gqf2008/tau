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

use std::sync::mpsc;
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
    fn resample_preserves_length_ratio() {
        let src = vec![0.5f32; 1000];
        let out = resample_linear(&src, 16_000, 48_000);
        assert_eq!(out.len(), 3000);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }
}
