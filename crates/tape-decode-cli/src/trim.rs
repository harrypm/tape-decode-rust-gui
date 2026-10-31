//! Lossless FLAC trimming: re-encode a sample range of a FLAC file into a new
//! FLAC file, preserving sample values bit-exactly (decode to native integers,
//! re-encode at the same bit depth), the channel count, and the header sample
//! rate. Used to drop the unrecorded lead-in/lead-out of RF captures, and to
//! cut the companion linear-audio capture over the same time range.

use std::fs::OpenOptions;
use std::io::{self, BufWriter, IsTerminal, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Frame, StreamInfo};
use flacenc::config::Encoder as EncoderConfig;
use flacenc::constant::MIN_BLOCK_SIZE;
use flacenc::encode_fixed_size_frame;
use flacenc::error::{Verified, Verify as _};
use flacenc::source::{Fill as _, FrameBuf};
use md5::{Digest as _, Md5};
use symphonia_bundle_flac::{FlacDecoder, FlacReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;

/// FLAC frame size used for the re-encoded output; 4096 keeps the output
/// within the FLAC subset for low header rates.
const BLOCK_SIZE: usize = 4096;

/// Largest sample count the 36-bit STREAMINFO `total_samples` field can hold.
/// Beyond it the length is declared unknown (0), as raw captures already do:
/// `flacenc` serializes the field with `write_lsbs(_, 36)`, which silently
/// keeps the low 36 bits. ~40 min at 28.636 MS/s, so RF captures reach it.
const MAX_DECLARABLE_SAMPLES: u64 = (1 << 36) - 1;

/// The value to write into STREAMINFO for a stream of `total_frames` samples:
/// the exact count when it fits, otherwise 0 ("unknown").
fn declared_total_samples(total_frames: u64) -> u64 {
    if total_frames > MAX_DECLARABLE_SAMPLES {
        0
    } else {
        total_frames
    }
}

// --- Bit-exact multi-channel FLAC reader --------------------------------------
//
// `crate::flac` normalizes to mono f32 for the decoder; trimming instead needs
// the native integers of any channel count, so this is a separate small reader.

pub struct RawFlacReader {
    reader: FlacReader<'static>,
    decoder: FlacDecoder,
    track_id: u32,
    pub sample_rate: u32,
    pub channels: usize,
    pub bits_per_sample: u32,
    shift: u32,
    /// Interleaved samples of the current packet not yet returned.
    pending: Vec<i32>,
    pending_pos: usize,
    /// Absolute per-channel sample index of the next frame `read` returns.
    position: u64,
    eof: bool,
    /// Set when decoding stopped on a mid-stream error (truncated capture).
    pub truncated: bool,
}

impl RawFlacReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let reader =
            FlacReader::try_new(mss, FormatOptions::default()).context("failed to read FLAC")?;
        let (track_id, params) = {
            let track = reader.tracks().first().context("FLAC has no tracks")?;
            let params = track
                .codec_params
                .as_ref()
                .and_then(CodecParameters::audio)
                .context("FLAC track is not audio")?
                .clone();
            (track.id, params)
        };
        let bits = params
            .bits_per_sample
            .context("FLAC stream info missing bits per sample")?;
        if !(4..=32).contains(&bits) {
            bail!("unsupported FLAC bit depth: {bits}");
        }
        let channels = params.channels.as_ref().map_or(1, |c| c.count());
        let sample_rate = params
            .sample_rate
            .context("FLAC stream info missing sample rate")?;
        let decoder = FlacDecoder::try_new(&params, &AudioDecoderOptions::default())
            .context("failed to initialize FLAC decoder")?;
        Ok(Self {
            reader,
            decoder,
            track_id,
            sample_rate,
            channels,
            bits_per_sample: bits,
            shift: 32 - bits,
            pending: Vec::new(),
            pending_pos: 0,
            position: 0,
            eof: false,
            truncated: false,
        })
    }

    /// Decode the next packet into `pending` (interleaved). A mid-stream
    /// error is treated as the end of usable data: RF captures routinely end
    /// on a truncated FLAC frame when the capture process was stopped.
    fn decode_next(&mut self) -> bool {
        let shift = self.shift;
        loop {
            let packet = match self.reader.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    self.eof = true;
                    return false;
                }
                Err(_) => {
                    self.eof = true;
                    self.truncated = true;
                    return false;
                }
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(_) => {
                    self.eof = true;
                    self.truncated = true;
                    return false;
                }
            };
            if decoded.frames() == 0 {
                continue;
            }
            let GenericAudioBufferRef::S32(buf) = decoded else {
                self.eof = true;
                self.truncated = true;
                return false;
            };
            let frames = buf.frames();
            self.pending.clear();
            self.pending.reserve(frames * self.channels);
            let planes: Vec<&[i32]> = (0..self.channels)
                .map(|ch| buf.plane(ch).expect("channel plane"))
                .collect();
            for frame in 0..frames {
                for plane in &planes {
                    self.pending.push(plane[frame] >> shift);
                }
            }
            self.pending_pos = 0;
            return true;
        }
    }

    fn fill(&mut self) -> bool {
        if self.pending_pos < self.pending.len() {
            return true;
        }
        if self.eof {
            return false;
        }
        self.decode_next()
    }

    /// Seek to the absolute per-channel sample `target` (exact): coarse seek
    /// to the containing packet, then drop samples up to the target.
    pub fn seek_to(&mut self, target: u64) -> Result<()> {
        if target != self.position {
            let ts = Timestamp::try_from(target).context("seek offset too large")?;
            if let Ok(seeked) = self.reader.seek(
                SeekMode::Coarse,
                SeekTo::Timestamp {
                    ts,
                    track_id: self.track_id,
                },
            ) {
                self.decoder.reset();
                self.pending.clear();
                self.pending_pos = 0;
                self.eof = false;
                self.position = seeked.actual_ts.get() as u64;
            }
            if self.position > target {
                bail!("FLAC seek overshot the trim start");
            }
        }
        while self.position < target {
            if !self.fill() {
                bail!("FLAC input ended before sample offset {target}");
            }
            let available = (self.pending.len() - self.pending_pos) / self.channels;
            let step = available.min((target - self.position) as usize);
            self.pending_pos += step * self.channels;
            self.position += step as u64;
        }
        Ok(())
    }

    /// Append up to `max_frames` interleaved frames to `out`; returns the
    /// number of frames appended (0 at end of data).
    pub fn read_frames(&mut self, out: &mut Vec<i32>, max_frames: usize) -> usize {
        let mut appended = 0usize;
        while appended < max_frames {
            if !self.fill() {
                break;
            }
            let available = (self.pending.len() - self.pending_pos) / self.channels;
            let step = available.min(max_frames - appended);
            let take = step * self.channels;
            out.extend_from_slice(&self.pending[self.pending_pos..self.pending_pos + take]);
            self.pending_pos += take;
            self.position += step as u64;
            appended += step;
        }
        appended
    }
}

// --- Parallel frame encoding -----------------------------------------------

/// One 4096-sample block awaiting encoding, tagged with its output position.
struct EncodeJob {
    frame_number: usize,
    samples: Vec<i32>,
}

type EncodeResult = std::result::Result<Frame, String>;

/// A single encoder worker: owns a bounded job queue (bounding how far the
/// main thread can read ahead) and an unbounded result queue (so the worker
/// never blocks handing back a finished frame).
struct Worker {
    job_tx: SyncSender<EncodeJob>,
    result_rx: Receiver<EncodeResult>,
    handle: JoinHandle<()>,
}

/// Blocks are handed to workers round-robin, so worker `n`'s results arrive
/// in the same order its jobs were sent (frame `i` is worker `i % N`'s
/// `i / N`-th job); collecting index `i` from worker `i % N` therefore
/// reconstructs the original order without a reorder buffer.
const WORKER_QUEUE_DEPTH: usize = 4;

impl Worker {
    fn spawn(config: Verified<EncoderConfig>, stream_info: StreamInfo, channels: usize) -> Self {
        let (job_tx, job_rx) = mpsc::sync_channel::<EncodeJob>(WORKER_QUEUE_DEPTH);
        let (result_tx, result_rx) = mpsc::channel::<EncodeResult>();
        let handle = thread::spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let frames = job.samples.len() / channels;
                let result = (|| -> EncodeResult {
                    let mut framebuf = FrameBuf::with_size(channels, frames)
                        .map_err(|err| format!("flacenc frame buffer rejected: {err}"))?;
                    framebuf
                        .fill_interleaved(&job.samples)
                        .map_err(|err| format!("flacenc fill failed: {err}"))?;
                    encode_fixed_size_frame(&config, &framebuf, job.frame_number, &stream_info)
                        .map_err(|err| format!("flacenc encode failed: {err:?}"))
                })();
                let failed = result.is_err();
                if result_tx.send(result).is_err() || failed {
                    break;
                }
            }
        });
        Self {
            job_tx,
            result_rx,
            handle,
        }
    }
}

// --- Streaming FLAC writer -----------------------------------------------------

struct FlacStreamWriter {
    out: BufWriter<std::fs::File>,
    info: StreamInfo,
    channels: usize,
    staging: Vec<i32>,
    workers: Vec<Worker>,
    /// Frame number of the next block to submit for encoding.
    next_frame_number: usize,
    /// Frame number of the next result to collect and write out; always
    /// `<= next_frame_number`.
    collected: usize,
    /// Maximum number of submitted-but-uncollected jobs kept in flight, so
    /// all workers stay fed while memory use stays bounded.
    lookahead: usize,
    total_frames: u64,
    /// STREAMINFO MD5, accumulated over the samples as they are submitted
    /// (independent of encode order). `flacenc`'s own `Context` cannot be
    /// used here: it zero-pads every `fill_interleaved` call up to a full
    /// block, which corrupts the digest on the short final block.
    md5: Md5,
    /// Scratch buffer reused across `submit_block` calls to batch MD5 input.
    md5_buf: Vec<u8>,
    bytes_per_sample: usize,
}

impl FlacStreamWriter {
    fn create(
        path: &Path,
        sample_rate: u32,
        channels: usize,
        bits: u32,
        overwrite: bool,
    ) -> Result<Self> {
        let mut open_options = OpenOptions::new();
        if overwrite {
            open_options.write(true).create(true).truncate(true);
        } else {
            open_options.write(true).create_new(true);
        }
        let file = open_options
            .open(path)
            .with_context(|| format!("failed to create output {}", path.display()))?;
        let mut out = BufWriter::new(file);
        // "fLaC", then a placeholder STREAMINFO block (rewritten in `finish`):
        // last-block flag set, type 0, length 34.
        out.write_all(b"fLaC")?;
        out.write_all(&[0x80, 0, 0, 34])?;
        out.write_all(&[0u8; 34])?;
        let config = EncoderConfig::default()
            .into_verified()
            .map_err(|(_, err)| anyhow::anyhow!("flacenc config rejected: {err}"))?;
        let info = StreamInfo::new(sample_rate as usize, channels, bits as usize)
            .map_err(|err| anyhow::anyhow!("flacenc stream info rejected: {err}"))?;
        let num_workers = thread::available_parallelism().map_or(1, |n| n.get());
        let workers: Vec<Worker> = (0..num_workers)
            .map(|_| Worker::spawn(config.clone(), info.clone(), channels))
            .collect();
        let lookahead = num_workers * WORKER_QUEUE_DEPTH;
        Ok(Self {
            out,
            info,
            channels,
            staging: Vec::new(),
            workers,
            next_frame_number: 0,
            collected: 0,
            lookahead,
            total_frames: 0,
            md5: Md5::new(),
            md5_buf: Vec::new(),
            bytes_per_sample: bits.div_ceil(8) as usize,
        })
    }

    fn push(&mut self, interleaved: &[i32]) -> Result<()> {
        self.staging.extend_from_slice(interleaved);
        while self.staging.len() >= BLOCK_SIZE * self.channels {
            let rest = self.staging.split_off(BLOCK_SIZE * self.channels);
            let block = std::mem::replace(&mut self.staging, rest);
            self.submit_block(block)?;
        }
        Ok(())
    }

    /// Hands a block to its round-robin worker, then drains any results that
    /// have fallen further behind than `lookahead` to bound memory use.
    fn submit_block(&mut self, interleaved: Vec<i32>) -> Result<()> {
        let frames = interleaved.len() / self.channels;
        // The digest covers the samples as little-endian two's-complement
        // integers of `bits_per_sample`, in interleaved order; recorded here
        // (submission order) rather than at collection, since encoding runs
        // out of order across workers. Hashed as one contiguous buffer rather
        // than per-sample: `Md5::update` calls carry enough fixed overhead
        // that a billion 1-byte calls (8-bit captures run at tens of
        // millions of samples/sec) become the bottleneck in their own right.
        self.md5_buf.clear();
        self.md5_buf.reserve(interleaved.len() * self.bytes_per_sample);
        for sample in &interleaved {
            self.md5_buf
                .extend_from_slice(&sample.to_le_bytes()[..self.bytes_per_sample]);
        }
        self.md5.update(&self.md5_buf);
        self.total_frames += frames as u64;
        let frame_number = self.next_frame_number;
        self.next_frame_number += 1;
        let worker = &self.workers[frame_number % self.workers.len()];
        worker
            .job_tx
            .send(EncodeJob {
                frame_number,
                samples: interleaved,
            })
            .map_err(|_| anyhow::anyhow!("flac encoder worker thread exited early"))?;
        while self.next_frame_number - self.collected > self.lookahead {
            self.collect_one()?;
        }
        Ok(())
    }

    /// Receives and writes out the next frame in order (`self.collected`).
    fn collect_one(&mut self) -> Result<()> {
        let worker = &self.workers[self.collected % self.workers.len()];
        let frame = worker
            .result_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("flac encoder worker thread exited early"))?
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        self.info.update_frame_info(&frame);
        let mut sink = ByteSink::new();
        frame
            .write(&mut sink)
            .map_err(|err| anyhow::anyhow!("flacenc serialize failed: {err}"))?;
        self.out.write_all(sink.as_slice())?;
        self.collected += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<u64> {
        if !self.staging.is_empty() {
            let frames = self.staging.len() / self.channels;
            if frames >= MIN_BLOCK_SIZE {
                let block = std::mem::take(&mut self.staging);
                self.submit_block(block)?;
            } else if self.next_frame_number == 0 {
                bail!(
                    "trim range is {frames} samples, under the {MIN_BLOCK_SIZE}-sample \
                     minimum FLAC block"
                );
            } else {
                // A fixed-block-size stream can only vary the length of its
                // last frame, so a tail this short can neither be merged into
                // the frame before it (that frame would exceed the block size
                // every decoder maps frame numbers through) nor encoded on its
                // own, `FrameBuf` refusing anything under MIN_BLOCK_SIZE. At
                // most 63 samples go, 2.2 us at 28.636 MS/s, against the 3 s
                // margin `--auto` already leaves at each end.
                tracing::warn!(
                    "dropping the last {} samples: under flacenc's {}-sample minimum block, \
                     and a fixed-block-size stream cannot merge them into the frame before",
                    frames,
                    MIN_BLOCK_SIZE,
                );
                self.staging.clear();
            }
        }
        while self.collected < self.next_frame_number {
            self.collect_one()?;
        }
        for worker in self.workers.drain(..) {
            drop(worker.job_tx);
            let _ = worker.handle.join();
        }
        let declared = declared_total_samples(self.total_frames);
        if declared == 0 && self.total_frames > 0 {
            tracing::warn!(
                "output is {} samples, over the 36-bit STREAMINFO counter ({} max); \
                 declaring the length unknown, as raw captures do. Readers that trust \
                 the header (libsndfile, and so hifi-decode) must stream it to EOF.",
                self.total_frames,
                MAX_DECLARABLE_SAMPLES,
            );
        }
        self.info.set_total_samples(declared as usize);
        // `update_frame_info` shrinks the minimum block size down to the short
        // final frame, which marks the stream as variable-block-size: decoders
        // then cannot map the fixed-block-size frame numbers the frames carry
        // back to sample positions, and the output stops being seekable. Every
        // frame but the last is a full block, so pin both sizes to it.
        self.info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(|err| anyhow::anyhow!("flacenc block sizes rejected: {err}"))?;
        self.info.set_md5_digest(&self.md5.clone().finalize().into());
        let mut sink = ByteSink::new();
        self.info
            .write(&mut sink)
            .map_err(|err| anyhow::anyhow!("flacenc stream info serialize failed: {err}"))?;
        let total = self.total_frames;
        self.out.flush()?;
        let mut file = self.out.into_inner().context("flush failed")?;
        file.seek(SeekFrom::Start(8))?;
        file.write_all(sink.as_slice())?;
        file.flush()?;
        Ok(total)
    }
}

// --- Progress reporting ----------------------------------------------------

/// Prints a single self-overwriting progress line to stderr while a cut runs.
/// Cuts run at tens of MiB/s over multi-GiB RF captures, so a silent CLI can
/// look hung; this is skipped when stderr isn't a terminal (piped/logged
/// runs), so it never litters non-interactive output with `\r` chatter.
struct Progress {
    enabled: bool,
    start: Instant,
    last: Instant,
    bytes_per_frame: u64,
    total_frames: Option<u64>,
}

impl Progress {
    fn new(bytes_per_frame: u64, total_frames: Option<u64>) -> Self {
        let now = Instant::now();
        Self {
            enabled: io::stderr().is_terminal(),
            start: now,
            last: now,
            bytes_per_frame,
            total_frames,
        }
    }

    fn tick(&mut self, frames_done: u64) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last) < Duration::from_millis(200) {
            return;
        }
        self.last = now;
        let elapsed = self.start.elapsed().as_secs_f64().max(0.001);
        let done_bytes = frames_done * self.bytes_per_frame;
        let rate_mib_s = done_bytes as f64 / elapsed / (1024.0 * 1024.0);
        match self.total_frames.filter(|&total| total > 0) {
            Some(total) => {
                let pct = (frames_done as f64 / total as f64 * 100.0).min(100.0);
                let remaining_bytes = total.saturating_sub(frames_done) * self.bytes_per_frame;
                let eta_secs = remaining_bytes as f64 / (rate_mib_s * 1024.0 * 1024.0).max(1.0);
                eprint!(
                    "\rtrimming: {pct:5.1}%  {rate_mib_s:6.1} MiB/s  eta {}    ",
                    format_duration(eta_secs as u64),
                );
            }
            None => {
                eprint!(
                    "\rtrimming: {:.2} GiB written  {rate_mib_s:6.1} MiB/s    ",
                    done_bytes as f64 / (1u64 << 30) as f64,
                );
            }
        }
        let _ = io::stderr().flush();
    }

    fn finish(&self) {
        if self.enabled {
            eprintln!();
        }
    }
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs / 60) % 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

// --- Cutting -------------------------------------------------------------------

pub struct CutStats {
    pub frames_written: u64,
    pub truncated_input: bool,
}

/// Copy `[start_frame, end_frame)` (per-channel samples; `end_frame == None`
/// means to the end of data) of `input` into `output`, re-encoded bit-exactly.
pub fn cut_flac(
    input: &Path,
    output: &Path,
    start_frame: u64,
    end_frame: Option<u64>,
    overwrite: bool,
) -> Result<CutStats> {
    let mut reader = RawFlacReader::open(input)?;
    let mut writer = FlacStreamWriter::create(
        output,
        reader.sample_rate,
        reader.channels,
        reader.bits_per_sample,
        overwrite,
    )?;
    reader.seek_to(start_frame)?;
    let total_frames = end_frame.map(|end| end.saturating_sub(start_frame));
    let mut remaining = total_frames;
    let bytes_per_frame =
        reader.channels as u64 * u64::from(reader.bits_per_sample.div_ceil(8));
    let mut progress = Progress::new(bytes_per_frame, total_frames);
    let mut frames_done = 0u64;
    let mut chunk: Vec<i32> = Vec::with_capacity(BLOCK_SIZE * 16 * reader.channels);
    loop {
        let want = match remaining {
            Some(0) => break,
            Some(left) => (BLOCK_SIZE * 16).min(left as usize),
            None => BLOCK_SIZE * 16,
        };
        chunk.clear();
        let got = reader.read_frames(&mut chunk, want);
        if got == 0 {
            break;
        }
        writer.push(&chunk)?;
        if let Some(left) = remaining.as_mut() {
            *left -= got as u64;
        }
        frames_done += got as u64;
        progress.tick(frames_done);
    }
    progress.finish();
    let frames_written = writer.finish()?;
    Ok(CutStats {
        frames_written,
        truncated_input: reader.truncated,
    })
}

/// Default output path: `<stem>_trimmed.flac` next to the input.
pub fn default_output_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    input.with_file_name(format!("{stem}_trimmed.flac"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_the_exact_length_when_it_fits() {
        assert_eq!(declared_total_samples(0), 0);
        assert_eq!(declared_total_samples(1), 1);
        assert_eq!(
            declared_total_samples(MAX_DECLARABLE_SAMPLES),
            MAX_DECLARABLE_SAMPLES
        );
    }

    #[test]
    fn declares_unknown_past_the_36_bit_counter() {
        assert_eq!(declared_total_samples(MAX_DECLARABLE_SAMPLES + 1), 0);
        assert_eq!(declared_total_samples(1 << 36), 0);
        // The capture that exposed this: 63.47 min at 28.636 MS/s, which the
        // bare `as usize` cast used to wrap to 40_340_523_264 (23.48 min),
        // stopping hifi-decode two thirds of the way into the tape.
        assert_eq!(declared_total_samples(109_060_000_000), 0);
    }

    /// Reads `total_samples` out of a serialized 34-byte STREAMINFO block:
    /// bytes 10..18 pack sample rate (20 bits), channels (3), bits per sample
    /// (5) and the 36-bit counter.
    fn total_samples_of(info: &[u8]) -> u64 {
        u64::from_be_bytes(info[10..18].try_into().unwrap()) & MAX_DECLARABLE_SAMPLES
    }

    /// Same, for a whole file: 4 bytes of "fLaC" marker, 4 of block header,
    /// then the block itself.
    fn declared_length_of(path: &Path) -> u64 {
        let bytes = std::fs::read(path).expect("read flac");
        assert_eq!(&bytes[..4], b"fLaC");
        total_samples_of(&bytes[8..42])
    }

    /// The over-long case can't be reached through `cut_flac` in a test (it
    /// takes 68 billion samples), so drive the tail of `finish` directly: the
    /// declared value goes through `set_total_samples` and out via `write`.
    #[test]
    fn over_long_stream_serializes_a_zero_length() {
        let mut info = StreamInfo::new(28_636, 1, 8).expect("stream info");
        info.set_total_samples(declared_total_samples(109_060_000_000) as usize);
        let mut sink = ByteSink::new();
        info.write(&mut sink).expect("serialize");
        assert_eq!(total_samples_of(sink.as_slice()), 0);

        // ...where the unguarded cast this replaces wrapped to 23.48 min.
        info.set_total_samples(109_060_000_000_usize);
        let mut sink = ByteSink::new();
        info.write(&mut sink).expect("serialize");
        assert_eq!(total_samples_of(sink.as_slice()), 40_340_523_264);
    }

    /// A scratch directory of its own per test: they share a process, so the
    /// pid alone would collide under the default parallel test runner.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tape-decode-trim-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Writes a mono 8-bit FLAC of `frames` samples: the shape `cut_flac` sees
    /// for an RF capture, at a size a test can afford.
    fn write_input(path: &Path, frames: u64) {
        let mut writer = FlacStreamWriter::create(path, 28_636, 1, 8, true).expect("create");
        let samples: Vec<i32> = (0..frames).map(|i| (i % 61) as i32 - 30).collect();
        writer.push(&samples).expect("push");
        assert_eq!(writer.finish().expect("finish"), frames);
    }

    #[test]
    fn short_cut_still_declares_its_exact_length() {
        // A short final block exercises the same `finish` path as a real cut.
        const FRAMES: u64 = BLOCK_SIZE as u64 * 3 + 1000;
        let dir = scratch_dir("exact-length");
        let (input, output) = (dir.join("in.flac"), dir.join("out.flac"));
        write_input(&input, FRAMES);

        let stats = cut_flac(&input, &output, 0, None, true).expect("cut");
        assert_eq!(stats.frames_written, FRAMES);
        assert_eq!(declared_length_of(&output), FRAMES);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A range leaving 1..63 samples past the last full block used to abort the
    /// whole cut, after the entire input had been read and re-encoded.
    #[test]
    fn sub_minimum_final_block_is_dropped_instead_of_failing() {
        const KEPT: u64 = BLOCK_SIZE as u64 * 2;
        let dir = scratch_dir("short-tail");
        let (input, output) = (dir.join("in.flac"), dir.join("out.flac"));
        write_input(&input, BLOCK_SIZE as u64 * 3);

        let stats = cut_flac(&input, &output, 0, Some(KEPT + 17), true).expect("cut");
        assert_eq!(stats.frames_written, KEPT);
        assert_eq!(declared_length_of(&output), KEPT);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// ...while a tail of exactly the minimum is kept whole.
    #[test]
    fn final_block_at_the_minimum_is_kept() {
        const FRAMES: u64 = BLOCK_SIZE as u64 + MIN_BLOCK_SIZE as u64;
        let dir = scratch_dir("min-tail");
        let (input, output) = (dir.join("in.flac"), dir.join("out.flac"));
        write_input(&input, BLOCK_SIZE as u64 * 2);

        let stats = cut_flac(&input, &output, 0, Some(FRAMES), true).expect("cut");
        assert_eq!(stats.frames_written, FRAMES);
        assert_eq!(declared_length_of(&output), FRAMES);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A whole cut under the minimum has no preceding frame to fall back on,
    /// so it has to fail rather than silently write a FLAC with no samples.
    #[test]
    fn cut_shorter_than_the_minimum_block_fails() {
        let dir = scratch_dir("degenerate");
        let (input, output) = (dir.join("in.flac"), dir.join("out.flac"));
        write_input(&input, BLOCK_SIZE as u64);

        // `.err().expect(..)` rather than `expect_err`: the latter would want
        // `CutStats: Debug` just to print a success that must not happen.
        let err = cut_flac(&input, &output, 0, Some(30), true)
            .err()
            .expect("cut must fail");
        assert!(
            err.to_string().contains("30 samples"),
            "unexpected error: {err}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
