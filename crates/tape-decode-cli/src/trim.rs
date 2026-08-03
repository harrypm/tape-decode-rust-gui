//! Lossless FLAC trimming: re-encode a sample range of a FLAC file into a new
//! FLAC file, preserving sample values bit-exactly (decode to native integers,
//! re-encode at the same bit depth), the channel count, and the header sample
//! rate. Used to drop the unrecorded lead-in/lead-out of RF captures, and to
//! cut the companion linear-audio capture over the same time range.

use std::fs::OpenOptions;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, StreamInfo};
use flacenc::config::Encoder as EncoderConfig;
use flacenc::encode_fixed_size_frame;
use flacenc::error::Verify as _;
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

// --- Streaming FLAC writer -----------------------------------------------------

struct FlacStreamWriter {
    out: BufWriter<std::fs::File>,
    config: flacenc::error::Verified<EncoderConfig>,
    info: StreamInfo,
    channels: usize,
    staging: Vec<i32>,
    frame_number: usize,
    total_frames: u64,
    /// STREAMINFO MD5, accumulated over the samples as they are encoded.
    /// `flacenc`'s own `Context` cannot be used here: it zero-pads every
    /// `fill_interleaved` call up to a full block, which corrupts the digest
    /// on the short final block.
    md5: Md5,
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
        Ok(Self {
            out,
            config,
            info,
            channels,
            staging: Vec::new(),
            frame_number: 0,
            total_frames: 0,
            md5: Md5::new(),
            bytes_per_sample: bits.div_ceil(8) as usize,
        })
    }

    fn push(&mut self, interleaved: &[i32]) -> Result<()> {
        self.staging.extend_from_slice(interleaved);
        while self.staging.len() >= BLOCK_SIZE * self.channels {
            let rest = self.staging.split_off(BLOCK_SIZE * self.channels);
            let block = std::mem::replace(&mut self.staging, rest);
            self.encode_block(&block)?;
        }
        Ok(())
    }

    fn encode_block(&mut self, interleaved: &[i32]) -> Result<()> {
        let frames = interleaved.len() / self.channels;
        let mut framebuf = FrameBuf::with_size(self.channels, frames)
            .map_err(|err| anyhow::anyhow!("flacenc frame buffer rejected: {err}"))?;
        framebuf
            .fill_interleaved(interleaved)
            .map_err(|err| anyhow::anyhow!("flacenc fill failed: {err}"))?;
        let frame = encode_fixed_size_frame(&self.config, &framebuf, self.frame_number, &self.info)
            .map_err(|err| anyhow::anyhow!("flacenc encode failed: {err:?}"))?;
        self.info.update_frame_info(&frame);
        // The digest covers the samples as little-endian two's-complement
        // integers of `bits_per_sample`, in interleaved order.
        for sample in interleaved {
            self.md5
                .update(&sample.to_le_bytes()[..self.bytes_per_sample]);
        }
        let mut sink = ByteSink::new();
        frame
            .write(&mut sink)
            .map_err(|err| anyhow::anyhow!("flacenc serialize failed: {err}"))?;
        self.out.write_all(sink.as_slice())?;
        self.frame_number += 1;
        self.total_frames += frames as u64;
        Ok(())
    }

    fn finish(mut self) -> Result<u64> {
        if !self.staging.is_empty() {
            let block = std::mem::take(&mut self.staging);
            self.encode_block(&block)?;
        }
        self.info.set_total_samples(self.total_frames as usize);
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
    let mut remaining = end_frame.map(|end| end.saturating_sub(start_frame));
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
    }
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
