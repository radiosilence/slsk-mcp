//! Spotting lossy audio passed off as lossless.
//!
//! A lossy encoder throws away the top of the spectrum before it does anything
//! else: LAME at 128 kbps low-passes near 16 kHz, V0 and 320 near 19.5–20 kHz.
//! Decoding that back to FLAC keeps the hole, and it is a *brickwall* — the
//! level falls tens of dB within a few hundred hertz — where a genuine master
//! either runs to Nyquist or rolls off gradually. So the test is where the
//! average spectrum ends and how abruptly.
//!
//! The same shape exposes fake hi-res: a 96 kHz file whose content stops dead
//! at 22 kHz was upsampled from CD. Zero-padded bit depth (a "24-bit" file
//! whose low eight bits are never set) is checked directly on the samples.
//!
//! None of this is proof. A quiet recording or a master that was band-limited
//! on purpose reads as a cutoff too, which is why every verdict carries a
//! confidence and a gradual roll-off is reported as uncertain rather than
//! lossy.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use rustfft::{FftPlanner, num_complex::Complex32};
use serde::{Deserialize, Serialize};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

const FFT: usize = 4096;
/// Spectrogram columns. Enough to see a track's structure at a glance.
const COLUMNS: u64 = 1000;
const IMAGE_HEIGHT: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, async_graphql::Enum)]
pub enum Verdict {
    /// Content reaches (close to) Nyquist.
    Lossless,
    /// A brickwall cutoff where a lossy encoder would put one.
    Lossy,
    /// Hi-res container, CD-rate content.
    Upsampled,
    /// Cutoff below Nyquist but gradual — a quiet or band-limited master, or
    /// a lossy source; the spectrum alone cannot tell.
    Uncertain,
    /// Too short, silent or undecodable.
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, async_graphql::SimpleObject)]
pub struct TrackAnalysis {
    pub file: String,
    pub sample_rate: u32,
    pub bits_per_sample: Option<u32>,
    /// Bits actually used: a 24-bit file padded from 16-bit reads 16.
    pub effective_bits: Option<u32>,
    pub duration_secs: f64,
    /// Where the spectrum ends, in Hz.
    pub cutoff_hz: Option<f64>,
    /// How far the level falls across the cutoff, in dB. A lossy encoder's
    /// low-pass is typically 30 dB or more within a kilohertz.
    pub drop_db: Option<f64>,
    pub verdict: Verdict,
    /// 0–1.
    pub confidence: f64,
    /// A likely source, where the cutoff suggests one ("~128 kbps MP3").
    pub estimate: Option<String>,
    #[graphql(skip)]
    pub spectrogram: Option<PathBuf>,
}

/// Analyse one file, writing its spectrogram to `spectrogram` if given.
///
/// CPU-bound for a few seconds per track; call it from a blocking thread.
pub fn analyse(path: &Path, spectrogram: Option<&Path>) -> Result<TrackAnalysis> {
    let decoded = decode(path)?;
    let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut analysis = classify(&decoded, file);
    if let Some(out) = spectrogram {
        if !decoded.columns.is_empty() {
            render(&decoded, analysis.cutoff_hz, out)?;
            analysis.spectrogram = Some(out.to_path_buf());
        }
    }
    Ok(analysis)
}

/// Album-level reading: the album is suspect if any track is confidently
/// lossy or upsampled, or if most of them are.
pub fn album_verdict(tracks: &[TrackAnalysis]) -> (Verdict, f64) {
    let decided: Vec<_> = tracks.iter().filter(|t| t.verdict != Verdict::Unknown).collect();
    if decided.is_empty() {
        return (Verdict::Unknown, 0.0);
    }
    for bad in [Verdict::Lossy, Verdict::Upsampled] {
        let hits: Vec<_> = decided.iter().filter(|t| t.verdict == bad).collect();
        let max = hits.iter().map(|t| t.confidence).fold(0.0, f64::max);
        if max >= 0.8 || hits.len() * 2 > decided.len() {
            return (bad, max);
        }
    }
    if decided.iter().filter(|t| t.verdict == Verdict::Uncertain).count() * 2 > decided.len() {
        return (Verdict::Uncertain, 0.4);
    }
    let min = decided.iter().map(|t| t.confidence).fold(1.0, f64::min);
    (Verdict::Lossless, min)
}

struct Decoded {
    sample_rate: u32,
    bits_per_sample: Option<u32>,
    /// OR of every sample, as i32. Trailing zeros are padding.
    bit_mask: u32,
    frames: u64,
    /// Power spectrum per column, dB, FFT/2 bins.
    columns: Vec<Vec<f32>>,
    /// Mean linear power per bin across all columns.
    mean_power: Vec<f64>,
}

fn decode(path: &Path) -> Result<Decoded> {
    let src = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .context("unsupported format")?;
    let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("no audio track"))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("no audio parameters"))?
        .clone();
    let track_id = track.id;
    let total_frames = track.num_frames;
    let sample_rate = params.sample_rate.ok_or_else(|| anyhow!("unknown sample rate"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .context("unsupported codec")?;

    let hop = match total_frames {
        Some(n) if n > FFT as u64 => ((n - FFT as u64) / COLUMNS).max(FFT as u64 / 4),
        _ => u64::from(sample_rate / 4),
    } as usize;

    let window: Vec<f32> = (0..FFT)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT - 1) as f32).cos())
        .collect();
    let window_power: f32 = window.iter().map(|w| w * w).sum::<f32>() * FFT as f32;
    let fft = FftPlanner::<f32>::new().plan_fft_forward(FFT);

    let mut pending: Vec<f32> = Vec::with_capacity(FFT * 2);
    // Absolute frame index of pending[0], and of the next column's start.
    let mut pending_start = 0usize;
    let mut next_column = 0usize;
    let mut frames = 0u64;
    let mut bit_mask = 0u32;
    let mut interleaved: Vec<i32> = Vec::new();
    let mut columns = Vec::new();
    let mut mean_power = vec![0f64; FFT / 2];
    let mut buf = vec![Complex32::default(); FFT];

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(a) => a,
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        let channels = audio.spec().channels().count().max(1);
        audio.copy_to_vec_interleaved(&mut interleaved);
        for frame in interleaved.chunks_exact(channels) {
            let mut sum = 0f32;
            for &s in frame {
                bit_mask |= s as u32;
                sum += s as f32 / i32::MAX as f32;
            }
            pending.push(sum / channels as f32);
        }
        frames += (interleaved.len() / channels) as u64;

        while next_column + FFT <= pending_start + pending.len() {
            let offset = next_column - pending_start;
            for (i, slot) in buf.iter_mut().enumerate() {
                *slot = Complex32::new(pending[offset + i] * window[i], 0.0);
            }
            fft.process(&mut buf);
            let column: Vec<f32> = buf[..FFT / 2]
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let p = c.norm_sqr() / window_power;
                    mean_power[i] += f64::from(p);
                    10.0 * (p + 1e-20).log10()
                })
                .collect();
            columns.push(column);
            next_column += hop;
            let drain = (next_column - pending_start).min(pending.len());
            pending.drain(..drain);
            pending_start += drain;
        }
    }

    if !columns.is_empty() {
        let n = columns.len() as f64;
        mean_power.iter_mut().for_each(|p| *p /= n);
    }
    Ok(Decoded {
        sample_rate,
        bits_per_sample: params.bits_per_sample,
        bit_mask,
        frames,
        columns,
        mean_power,
    })
}

fn classify(d: &Decoded, file: String) -> TrackAnalysis {
    let nyquist = f64::from(d.sample_rate) / 2.0;
    let bin_hz = f64::from(d.sample_rate) / FFT as f64;
    let effective_bits = (d.bit_mask != 0).then(|| 32 - d.bit_mask.trailing_zeros());
    let mut out = TrackAnalysis {
        file,
        sample_rate: d.sample_rate,
        bits_per_sample: d.bits_per_sample,
        effective_bits,
        duration_secs: d.frames as f64 / f64::from(d.sample_rate),
        cutoff_hz: None,
        drop_db: None,
        verdict: Verdict::Unknown,
        confidence: 0.0,
        estimate: None,
        spectrogram: None,
    };
    if d.columns.len() < 8 {
        return out;
    }

    let db: Vec<f64> = d.mean_power.iter().map(|p| 10.0 * (p + 1e-20).log10()).collect();
    let smooth = moving_average(&db, (150.0 / bin_hz).ceil() as usize);
    let band = |lo: f64, hi: f64| {
        let (a, b) = ((lo / bin_hz) as usize, ((hi / bin_hz) as usize).min(smooth.len()));
        if a >= b { f64::NAN } else { smooth[a..b].iter().sum::<f64>() / (b - a) as f64 }
    };

    let reference = band(200.0, 4000.0);
    if !reference.is_finite() || reference < -100.0 {
        return out;
    }
    // The floor is whatever sits in the top sliver of the spectrum: dither or
    // noise in a real master, near-digital-silence under a lossy low-pass.
    let floor = band(nyquist * 0.97, nyquist);
    let threshold = floor.max(reference - 90.0) + 10.0;
    // Content running flat to Nyquist leaves no edge above the floor at all.
    let cutoff_bin = (0..smooth.len())
        .rev()
        .find(|&i| smooth[i] > threshold)
        .unwrap_or(smooth.len() - 1);
    let cutoff = cutoff_bin as f64 * bin_hz;
    let drop = band(cutoff - 1500.0, cutoff - 300.0) - band(cutoff + 300.0, (cutoff + 1500.0).min(nyquist));
    let drop = if drop.is_finite() { drop } else { 0.0 };
    out.cutoff_hz = Some(cutoff.round());
    out.drop_db = Some((drop * 10.0).round() / 10.0);
    let steep = drop >= 25.0;

    if let (Some(bits), Some(eff)) = (d.bits_per_sample, effective_bits) {
        if bits > 16 && eff <= 16 {
            out.verdict = Verdict::Upsampled;
            out.confidence = 0.95;
            out.estimate = Some(format!("{bits}-bit container, 16-bit content"));
            return out;
        }
    }

    if d.sample_rate > 48_000 && cutoff <= 24_500.0 && steep {
        out.verdict = Verdict::Upsampled;
        out.confidence = 0.9;
        out.estimate = Some(if cutoff < 20_500.0 {
            "lossy source, resampled to hi-res".into()
        } else {
            "44.1/48 kHz source, resampled".into()
        });
        return out;
    }

    // Full band: anything within the last few percent before Nyquist, or the
    // CD anti-alias filter at 20.5–22 kHz, which lossy encoders never reach.
    let full_band = if d.sample_rate > 48_000 { 24_500.0 } else { (nyquist * 0.95).min(20_900.0) };
    if cutoff >= full_band {
        out.verdict = Verdict::Lossless;
        out.confidence = if drop < 40.0 { 0.9 } else { 0.8 };
        return out;
    }
    if !steep {
        out.verdict = Verdict::Uncertain;
        out.confidence = 0.4;
        out.estimate = Some("gradual roll-off".into());
        return out;
    }
    let (confidence, estimate) = match cutoff {
        c if c < 15_500.0 => (0.95, "≤128 kbps lossy"),
        c if c < 17_500.0 => (0.9, "~128–192 kbps lossy"),
        c if c < 19_000.0 => (0.8, "~192–256 kbps lossy"),
        _ => (0.6, "~V0/320 kbps lossy"),
    };
    out.verdict = Verdict::Lossy;
    out.confidence = confidence;
    out.estimate = Some(estimate.into());
    out
}

fn moving_average(xs: &[f64], width: usize) -> Vec<f64> {
    let half = width.max(1) / 2;
    (0..xs.len())
        .map(|i| {
            let (a, b) = (i.saturating_sub(half), (i + half + 1).min(xs.len()));
            xs[a..b].iter().sum::<f64>() / (b - a) as f64
        })
        .collect()
}

/// Frequency on the vertical axis, linear to Nyquist, as Spek draws it — the
/// cutoff a reader is looking for is a horizontal edge near the top.
fn render(d: &Decoded, cutoff: Option<f64>, out: &Path) -> Result<()> {
    let width = d.columns.len();
    let bins = FFT / 2;
    let max = d.columns.iter().flatten().copied().fold(f32::MIN, f32::max);
    let min = max - 110.0;
    let nyquist = d.sample_rate as f32 / 2.0;
    let mut pixels = vec![0u8; width * IMAGE_HEIGHT * 3];
    for (x, column) in d.columns.iter().enumerate() {
        for y in 0..IMAGE_HEIGHT {
            let row = IMAGE_HEIGHT - 1 - y;
            let (a, b) = (y * bins / IMAGE_HEIGHT, ((y + 1) * bins / IMAGE_HEIGHT).max(y * bins / IMAGE_HEIGHT + 1));
            let v = column[a..b].iter().copied().fold(f32::MIN, f32::max);
            let [r, g, bl] = colour(((v - min) / (max - min)).clamp(0.0, 1.0));
            let i = (row * width + x) * 3;
            pixels[i..i + 3].copy_from_slice(&[r, g, bl]);
        }
    }
    // Faint rules every 4 kHz so the cutoff can be read off by eye.
    let mut khz = 4000.0;
    while khz < nyquist {
        let row = IMAGE_HEIGHT - 1 - ((khz / nyquist) * IMAGE_HEIGHT as f32) as usize;
        for x in (0..width).step_by(3) {
            let i = (row * width + x) * 3;
            pixels[i..i + 3].copy_from_slice(&[90, 90, 90]);
        }
        khz += 4000.0;
    }
    if let Some(c) = cutoff {
        let row = IMAGE_HEIGHT - 1 - (((c as f32) / nyquist).min(1.0) * (IMAGE_HEIGHT - 1) as f32) as usize;
        for x in (0..width).step_by(6) {
            let i = (row * width + x) * 3;
            pixels[i..i + 3].copy_from_slice(&[80, 220, 255]);
        }
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::io::BufWriter::new(std::fs::File::create(out)?);
    let mut encoder = png::Encoder::new(file, width as u32, IMAGE_HEIGHT as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&pixels)?;
    Ok(())
}

/// Black → purple → red → orange → yellow → white.
fn colour(t: f32) -> [u8; 3] {
    const STOPS: [(f32, [f32; 3]); 6] = [
        (0.0, [0.0, 0.0, 0.0]),
        (0.25, [70.0, 0.0, 110.0]),
        (0.5, [200.0, 20.0, 60.0]),
        (0.7, [250.0, 120.0, 0.0]),
        (0.88, [255.0, 230.0, 60.0]),
        (1.0, [255.0, 255.0, 255.0]),
    ];
    let i = STOPS.iter().rposition(|(s, _)| *s <= t).unwrap_or(0).min(STOPS.len() - 2);
    let ((s0, c0), (s1, c1)) = (STOPS[i], STOPS[i + 1]);
    let f = ((t - s0) / (s1 - s0)).clamp(0.0, 1.0);
    [0, 1, 2].map(|k| (c0[k] + (c1[k] - c0[k]) * f) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    /// White noise, optionally brickwalled at `cutoff` Hz in the frequency
    /// domain — the shape a lossy encoder's low-pass leaves.
    fn noise(seconds: usize, cutoff: Option<f64>) -> Vec<i16> {
        let n = (RATE as usize * seconds).next_power_of_two();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut signal: Vec<Complex32> = (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                Complex32::new((state as f64 / u64::MAX as f64 - 0.5) as f32, 0.0)
            })
            .collect();
        if let Some(cutoff) = cutoff {
            let mut planner = FftPlanner::<f32>::new();
            planner.plan_fft_forward(n).process(&mut signal);
            let bin = (cutoff / f64::from(RATE) * n as f64) as usize;
            for (i, s) in signal.iter_mut().enumerate() {
                if i > bin && i < n - bin {
                    *s = Complex32::default();
                }
            }
            planner.plan_fft_inverse(n).process(&mut signal);
            signal.iter_mut().for_each(|s| *s /= n as f32);
        }
        signal.iter().map(|s| (s.re * 20_000.0) as i16).collect()
    }

    fn wav(dir: &Path, name: &str, samples: &[i16]) -> PathBuf {
        let path = dir.join(name);
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&RATE.to_le_bytes());
        bytes.extend_from_slice(&(RATE * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slsk-analysis-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn full_band_noise_is_lossless() {
        let dir = tmp();
        let a = analyse(&wav(&dir, "full.wav", &noise(8, None)), Some(&dir.join("full.png"))).unwrap();
        assert_eq!(a.verdict, Verdict::Lossless, "{a:?}");
        assert!(dir.join("full.png").exists());
    }

    #[test]
    fn a_16khz_brickwall_reads_as_low_bitrate_lossy() {
        let dir = tmp();
        let a = analyse(&wav(&dir, "lp.wav", &noise(8, Some(16_000.0))), None).unwrap();
        assert_eq!(a.verdict, Verdict::Lossy, "{a:?}");
        let cutoff = a.cutoff_hz.unwrap();
        assert!((15_500.0..16_500.0).contains(&cutoff), "{cutoff}");
        assert!(a.confidence >= 0.9);
    }

    #[test]
    fn a_19_5khz_brickwall_is_lossy_with_less_confidence() {
        let dir = tmp();
        let a = analyse(&wav(&dir, "v0.wav", &noise(8, Some(19_500.0))), None).unwrap();
        assert_eq!(a.verdict, Verdict::Lossy, "{a:?}");
        assert!(a.confidence < 0.8);
    }

    #[test]
    fn one_confident_lossy_track_condemns_the_album() {
        let track = |verdict, confidence| TrackAnalysis {
            file: String::new(),
            sample_rate: RATE,
            bits_per_sample: Some(16),
            effective_bits: Some(16),
            duration_secs: 1.0,
            cutoff_hz: None,
            drop_db: None,
            verdict,
            confidence,
            estimate: None,
            spectrogram: None,
        };
        let tracks = [track(Verdict::Lossless, 0.9), track(Verdict::Lossless, 0.9), track(Verdict::Lossy, 0.95)];
        assert_eq!(album_verdict(&tracks).0, Verdict::Lossy);
        let tracks = [track(Verdict::Lossless, 0.9), track(Verdict::Lossless, 0.9), track(Verdict::Lossy, 0.6)];
        assert_eq!(album_verdict(&tracks).0, Verdict::Lossless);
    }
}
