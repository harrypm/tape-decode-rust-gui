//! Signal-presence scanning: maps recorded content against unrecorded
//! (carrier-less) tape in an RF capture, without decoding video.
//!
//! The discriminator is the power contrast between the luma FM carrier band
//! (derived from the decode profile) and a high reference band that no video
//! format occupies. Measured on real captures (Video8 and VHS), recorded tape
//! shows +20 to +29 dB of contrast while unrecorded tape sits at ~0 dB, so the
//! thresholds below have a wide margin on both sides.

use std::fs::OpenOptions;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use tape_decode::DecodeProfile;

use crate::reader::{open_source, SampleFormat, SampleSource};

/// Contrast above which a window is recorded tape, in dB.
const SIGNAL_ENTER_DB: f64 = 8.0;
/// Contrast below which a window is unrecorded tape, in dB; between the two
/// thresholds the previous state persists (hysteresis).
const SIGNAL_EXIT_DB: f64 = 4.0;
/// Single threshold used when refining a transition position.
const SIGNAL_MID_DB: f64 = 6.0;
/// Analysis window, seconds of tape.
const WINDOW_SECS: f64 = 0.1;
/// Fraction of each window discarded while the IIR filters settle.
const WARMUP_FRACTION: f64 = 0.25;

pub struct ScanParams {
    /// True input sample rate in Hz (the FLAC header rate tag is unreliable).
    pub sample_rate_hz: f64,
    /// Distance between probes, seconds.
    pub stride_secs: f64,
    /// Signal or blank events shorter than this are merged into their
    /// neighbours (dropouts, head-switch noise, RF flapping at cuts).
    pub min_event_secs: f64,
    /// Margin applied outward around the recorded span for the suggested trim
    /// bounds, seconds.
    pub margin_secs: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub is_signal: bool,
    /// Absolute sample bounds, end exclusive.
    pub start: u64,
    pub end: u64,
}

pub struct ScanReport {
    pub sample_rate_hz: f64,
    /// End of usable capture data in samples (a truncated FLAC tail counts as
    /// the end of the capture, not an error).
    pub capture_end: u64,
    pub segments: Vec<Segment>,
    /// Suggested trim bounds (margin applied), when any signal was found.
    pub suggested: Option<(u64, u64)>,
}

impl ScanReport {
    pub fn signal_bounds(&self) -> Option<(u64, u64)> {
        let first = self.segments.iter().find(|s| s.is_signal)?.start;
        let last = self.segments.iter().rev().find(|s| s.is_signal)?.end;
        Some((first, last))
    }
}

// --- Band-power measurement --------------------------------------------------

/// One RBJ cookbook biquad, direct form II transposed, f64 state.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// Butterworth section: `q` selects the section of the cascade.
    fn new(cutoff_hz: f64, fs: f64, q: f64, highpass: bool) -> Self {
        let omega = 2.0 * std::f64::consts::PI * cutoff_hz / fs;
        let (sin, cos) = omega.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let (b0, b1) = if highpass {
            ((1.0 + cos) / 2.0, -(1.0 + cos))
        } else {
            ((1.0 - cos) / 2.0, 1.0 - cos)
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// 4th-order Butterworth band-pass as HP+LP cascades (two biquads each, with
/// the standard Butterworth section Qs).
struct BandPass {
    sections: [Biquad; 4],
}

const BUTTERWORTH4_Q: [f64; 2] = [0.541_196_100_146_197, 1.306_562_964_876_376];

impl BandPass {
    fn new(low_hz: f64, high_hz: f64, fs: f64) -> Self {
        Self {
            sections: [
                Biquad::new(low_hz, fs, BUTTERWORTH4_Q[0], true),
                Biquad::new(low_hz, fs, BUTTERWORTH4_Q[1], true),
                Biquad::new(high_hz, fs, BUTTERWORTH4_Q[0], false),
                Biquad::new(high_hz, fs, BUTTERWORTH4_Q[1], false),
            ],
        }
    }

    /// Mean power of the band over `samples`, skipping the filter warmup.
    fn power(&mut self, samples: &[f32], warmup: usize) -> f64 {
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for (index, &sample) in samples.iter().enumerate() {
            let mut value = f64::from(sample);
            for section in &mut self.sections {
                value = section.process(value);
            }
            if index >= warmup {
                sum += value * value;
                count += 1;
            }
        }
        if count == 0 {
            0.0
        } else {
            sum / count as f64
        }
    }
}

/// Frequency bands used by the classifier, derived from the decode profile.
pub struct ScanBands {
    pub signal_low_hz: f64,
    pub signal_high_hz: f64,
    pub noise_low_hz: f64,
    pub noise_high_hz: f64,
}

/// Profile frequency fields mix Hz and MHz (e.g. VIDEO8's `video_rf_peak.freq`
/// is `4.8` while SECAM_VHS's is `4300000.0`); normalize to Hz.
fn to_hz(value: f64) -> f64 {
    if value < 1000.0 {
        value * 1.0e6
    } else {
        value
    }
}

impl ScanBands {
    pub fn from_profile(profile: &DecodeProfile, fs: f64) -> Result<Self> {
        let params = &profile.decoder_params;
        let carrier = match &params.video_rf_peak {
            Some(peak) => to_hz(peak.freq),
            // No peaking filter in the profile: fall back to the middle of the
            // extra band-limiting filters, which bracket the FM spectrum.
            None => (to_hz(params.video_hpf_extra) + to_hz(params.video_lpf_extra)) / 2.0,
        };
        let signal_low_hz = 0.65 * carrier;
        let signal_high_hz = 1.15 * carrier;
        let mut noise_low_hz = 0.35 * fs;
        let mut noise_high_hz = 0.45 * fs;
        // Keep the reference band clear of the format's own spectrum.
        let spectrum_top = to_hz(params.video_lpf_extra).max(signal_high_hz) + 0.5e6;
        if noise_low_hz < spectrum_top {
            noise_low_hz = spectrum_top;
            noise_high_hz = (spectrum_top + 0.1 * fs).min(0.47 * fs);
        }
        if signal_high_hz >= 0.47 * fs {
            bail!(
                "sample rate {:.1} MHz is too low to cover the {:.1} MHz luma carrier",
                fs / 1e6,
                carrier / 1e6
            );
        }
        if noise_low_hz >= noise_high_hz {
            bail!(
                "sample rate {:.1} MHz leaves no clean reference band above the video spectrum",
                fs / 1e6
            );
        }
        Ok(Self {
            signal_low_hz,
            signal_high_hz,
            noise_low_hz,
            noise_high_hz,
        })
    }
}

// --- Probing -----------------------------------------------------------------

struct Prober {
    source: Box<dyn SampleSource>,
    bands: ScanBands,
    fs: f64,
    window: usize,
    buffer: Vec<f32>,
}

impl Prober {
    /// Contrast in dB at `sample`, or `None` when the data there cannot be
    /// read (past the end of the capture, or inside a corrupted FLAC tail).
    fn contrast_at(&mut self, sample: u64) -> Option<f64> {
        if self.source.seek_samples(sample).is_err() {
            return None;
        }
        let got = match self.source.read(&mut self.buffer) {
            Ok(got) => got,
            Err(_) => return None,
        };
        if got < self.buffer.len() {
            return None;
        }
        Some(self.contrast_of_buffer())
    }

    fn contrast_of_buffer(&mut self) -> f64 {
        let warmup = (self.window as f64 * WARMUP_FRACTION) as usize;
        let mut signal_band =
            BandPass::new(self.bands.signal_low_hz, self.bands.signal_high_hz, self.fs);
        let mut noise_band =
            BandPass::new(self.bands.noise_low_hz, self.bands.noise_high_hz, self.fs);
        let ps = signal_band.power(&self.buffer, warmup);
        let pn = noise_band.power(&self.buffer, warmup);
        10.0 * ((ps + f64::MIN_POSITIVE) / (pn + f64::MIN_POSITIVE)).log10()
    }

    /// Last sample position at which a full window is still readable,
    /// found by exponential growth then binary search.
    fn find_capture_end(&mut self) -> Result<u64> {
        let mut lo = 0u64;
        if self.contrast_at(lo).is_none() {
            bail!("input has less than one analysis window of readable data");
        }
        let mut hi = (self.fs * 60.0) as u64;
        while self.contrast_at(hi).is_some() {
            lo = hi;
            hi = hi.saturating_mul(2);
            if hi > 1 << 46 {
                bail!("could not find the end of the capture (input beyond ~2000 hours?)");
            }
        }
        // Invariant: readable at lo, not readable at hi.
        while hi - lo > self.window as u64 {
            let mid = lo + (hi - lo) / 2;
            if self.contrast_at(mid).is_some() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(lo + self.window as u64)
    }
}

// --- Scan driver ---------------------------------------------------------------

pub fn scan_file(
    path: &Path,
    format: SampleFormat,
    profile: &DecodeProfile,
    params: &ScanParams,
) -> Result<ScanReport> {
    if path.as_os_str() == "-" {
        bail!("scan needs a seekable file, not standard input");
    }
    let fs = params.sample_rate_hz;
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to open input {}", path.display()))?;
    let source = open_source(file, format)?;
    let bands = ScanBands::from_profile(profile, fs)?;
    tracing::info!(
        "scan bands: signal {:.2}-{:.2} MHz, reference {:.2}-{:.2} MHz",
        bands.signal_low_hz / 1e6,
        bands.signal_high_hz / 1e6,
        bands.noise_low_hz / 1e6,
        bands.noise_high_hz / 1e6,
    );
    let window = (fs * WINDOW_SECS) as usize;
    let mut prober = Prober {
        source,
        bands,
        fs,
        window,
        buffer: vec![0.0; window],
    };

    tracing::info!("locating end of capture (probing forward from a 60s guess)...");
    let capture_end = prober.find_capture_end()?;
    let stride = (params.stride_secs * fs) as u64;
    let stride = stride.max(window as u64);
    let probe_count = (capture_end / stride) + 1;
    tracing::info!(
        "scanning {} of tape ({probe_count} probes at {:.1}s stride)",
        format_tape_time(capture_end, fs),
        params.stride_secs,
    );

    // Coarse classification on the grid, with hysteresis.
    let mut grid: Vec<(u64, bool)> = Vec::new();
    let mut state: Option<bool> = None;
    let mut position = 0u64;
    let scan_start = std::time::Instant::now();
    let mut last_report = scan_start;
    while position + window as u64 <= capture_end {
        if let Some(contrast) = prober.contrast_at(position) {
            let is_signal = match state {
                Some(true) => contrast > SIGNAL_EXIT_DB,
                Some(false) => contrast > SIGNAL_ENTER_DB,
                None => contrast > SIGNAL_MID_DB,
            };
            state = Some(is_signal);
            grid.push((position, is_signal));
        }
        position += stride;
        let now = std::time::Instant::now();
        if now.duration_since(last_report).as_secs_f64() >= 5.0 {
            last_report = now;
            let fraction = position as f64 / capture_end as f64;
            let elapsed = now.duration_since(scan_start).as_secs_f64();
            let eta_secs = if fraction > 0.0 {
                elapsed / fraction - elapsed
            } else {
                f64::NAN
            };
            tracing::info!(
                "scanning: {} / {} ({:.1}%), elapsed {:.0}s, eta {:.0}s",
                format_tape_time(position, fs),
                format_tape_time(capture_end, fs),
                fraction * 100.0,
                elapsed,
                eta_secs,
            );
        }
    }
    if grid.is_empty() {
        bail!("capture too short to scan");
    }

    // Refine each transition down to window resolution.
    let mut segments: Vec<Segment> = Vec::new();
    let mut current = grid[0].1;
    let mut current_start = 0u64;
    for pair in grid.windows(2) {
        let (left, left_signal) = pair[0];
        let (right, right_signal) = pair[1];
        if left_signal == right_signal {
            continue;
        }
        // Binary search for the transition point in (left, right].
        let (mut lo, mut hi) = (left, right);
        while hi - lo > window as u64 {
            let mid = lo + (hi - lo) / 2;
            let mid_signal = prober
                .contrast_at(mid)
                .map(|c| c > SIGNAL_MID_DB)
                .unwrap_or(false);
            if mid_signal == left_signal {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        segments.push(Segment {
            is_signal: current,
            start: current_start,
            end: hi,
        });
        current = right_signal;
        current_start = hi;
    }
    segments.push(Segment {
        is_signal: current,
        start: current_start,
        end: capture_end,
    });

    // Merge events shorter than the minimum into their surroundings (dropouts,
    // head-switch noise, flapping RF at recording cuts), then coalesce equal
    // neighbours. Each pass removes a segment, so this terminates.
    let min_event = (params.min_event_secs * fs) as u64;
    while segments.len() > 1 {
        let Some(index) = segments
            .iter()
            .position(|segment| segment.end - segment.start < min_event)
        else {
            break;
        };
        let removed = segments.remove(index);
        if index == 0 {
            segments[0].start = removed.start;
        } else {
            segments[index - 1].end = removed.end;
        }
        let mut cursor = 1;
        while cursor < segments.len() {
            if segments[cursor - 1].is_signal == segments[cursor].is_signal {
                segments[cursor - 1].end = segments[cursor].end;
                segments.remove(cursor);
            } else {
                cursor += 1;
            }
        }
    }

    let margin = (params.margin_secs * fs) as u64;
    let report = ScanReport {
        sample_rate_hz: fs,
        capture_end,
        suggested: None,
        segments,
    };
    let suggested = report.signal_bounds().map(|(first, last)| {
        (
            first.saturating_sub(margin),
            (last + margin).min(capture_end),
        )
    });
    Ok(ScanReport {
        suggested,
        ..report
    })
}

// --- Reporting -----------------------------------------------------------------

pub fn format_tape_time(samples: u64, fs: f64) -> String {
    let secs = samples as f64 / fs;
    let hours = (secs / 3600.0) as u64;
    let minutes = ((secs / 60.0) as u64) % 60;
    let rem = secs - (hours * 3600 + minutes * 60) as f64;
    format!("{hours}:{minutes:02}:{rem:04.1}")
}

pub fn print_report(report: &ScanReport) {
    let fs = report.sample_rate_hz;
    let mut effective = 0u64;
    for (index, segment) in report.segments.iter().enumerate() {
        let kind = if segment.is_signal {
            "signal"
        } else if index == 0 {
            "lead-in"
        } else if index == report.segments.len() - 1 {
            "lead-out"
        } else {
            "blank"
        };
        println!(
            "{kind:9} {} -> {}  ({})",
            format_tape_time(segment.start, fs),
            format_tape_time(segment.end, fs),
            format_tape_time(segment.end - segment.start, fs),
        );
        if segment.is_signal {
            effective += segment.end - segment.start;
        }
    }
    println!(
        "capture end     {}",
        format_tape_time(report.capture_end, fs)
    );
    println!("effective video {}", format_tape_time(effective, fs));
    if let Some((start, end)) = report.suggested {
        println!(
            "suggested trim  --start {:.2} --end {:.2}  (keeps {})",
            start as f64 / fs,
            end as f64 / fs,
            format_tape_time(end - start, fs),
        );
    } else {
        println!("no recorded content found");
    }
}

pub fn report_to_json(report: &ScanReport) -> serde_json::Value {
    let fs = report.sample_rate_hz;
    serde_json::json!({
        "sampleRateHz": fs,
        "captureEnd": { "samples": report.capture_end, "seconds": report.capture_end as f64 / fs },
        "segments": report.segments.iter().map(|s| serde_json::json!({
            "kind": if s.is_signal { "signal" } else { "blank" },
            "startSamples": s.start,
            "endSamples": s.end,
            "startSeconds": s.start as f64 / fs,
            "endSeconds": s.end as f64 / fs,
        })).collect::<Vec<_>>(),
        "suggestedTrim": report.suggested.map(|(start, end)| serde_json::json!({
            "startSamples": start,
            "endSamples": end,
            "startSeconds": start as f64 / fs,
            "endSeconds": end as f64 / fs,
        })),
    })
}
