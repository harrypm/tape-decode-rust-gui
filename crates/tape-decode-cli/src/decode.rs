use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, bail, Result};
use tape_decode::{Decoder, DecoderMetadata, DecoderSpec, LumaOutput, WriteableField, BLOCKSIZE};

use crate::reader::DecodeReader;
use crate::writer::DecodeWriter;

/// Stream `reader` from its current position (the stream start) through
/// `decoder`, handing every batch of decoded fields to `sink` until the input
/// ends or `sink` returns false.
fn decode_serial(
    reader: &mut DecodeReader,
    decoder: &mut Decoder,
    spec: &DecoderSpec,
    mut sink: impl FnMut(&[WriteableField], Option<&DecoderMetadata>) -> Result<bool>,
) -> Result<()> {
    // Feed the decoder one chunk at a time over a sliding window starting at
    // absolute sample `base` (0, since reading begins at the stream start). A
    // chunk exceeds one field's span, so each refill makes progress. `decode`
    // reports the offset before which input is no longer needed; we drop that
    // prefix, or seek forward past it when `start_offset` lands beyond the window.
    let chunk = spec.readlen() + 4 * BLOCKSIZE;
    let mut window: Vec<f32> = Vec::new();
    let mut read_buffer = vec![0.0f32; chunk];
    let mut base: u64 = 0;
    let mut read_pos: u64 = 0;

    loop {
        let read = reader.read(&mut read_buffer)?;
        window.extend_from_slice(&read_buffer[..read]);
        let final_chunk = read < chunk;
        read_pos += read as u64;

        let (consumed, fields) = decoder.decode(&window, base, final_chunk);
        if !sink(&fields, decoder.metadata().as_ref())? || final_chunk {
            return Ok(());
        }
        if consumed > read_pos {
            // The decoder skipped past everything buffered (an --offset, a
            // stretch without sync): move the input forward and start a fresh
            // window there; an input that ends first reports it on the next
            // read.
            reader.seek_samples(consumed)?;
            read_pos = consumed;
            window.clear();
        } else {
            window.drain(..(consumed - base) as usize);
        }
        base = consumed;
    }
}

/// Decode the whole input serially. Like the multithreaded path, the input is
/// streamed once from the stream start and the decoder skips past everything
/// before `start_offset` itself (so this works on non-seekable inputs such as
/// pipes); `start_offset` is taken directly rather than inferred from the
/// reader's position.
pub fn decode_all(
    reader: &mut DecodeReader,
    writer: &mut DecodeWriter,
    spec: Arc<DecoderSpec>,
    start_offset: u64,
) -> Result<()> {
    let mut decoder = Decoder::new(Arc::clone(&spec), start_offset);
    let mut fields_written = 0usize;
    decode_serial(reader, &mut decoder, &spec, |fields, metadata| {
        for field in fields {
            writer.write_writeable(field, metadata)?;
        }
        fields_written += fields.len();
        Ok(true)
    })?;

    if fields_written != 0 {
        tracing::info!("Completed: saving JSON and exiting.");
    } else {
        tracing::info!("Completed without handling any frames.");
    }

    writer.close(decoder.metadata())?;
    Ok(())
}

/// Calibration pass for the ME-SECAM carrier servo: decode up to `max_fields`
/// fields from `start_offset`, discarding the pictures, and report the
/// conversion LO trim the servo converged to (`None` without a usable carrier
/// measurement). The reader is left where the pass stopped; the caller rewinds
/// it before the real decode.
pub fn calibrate_secam_lo_trim(
    reader: &mut DecodeReader,
    spec: Arc<DecoderSpec>,
    start_offset: u64,
    max_fields: usize,
) -> Result<Option<f64>> {
    let mut decoder = Decoder::new(Arc::clone(&spec), start_offset);
    let mut decoded = 0usize;
    decode_serial(reader, &mut decoder, &spec, |fields, _| {
        decoded += fields.len();
        Ok(decoded < max_fields)
    })?;
    Ok(decoder.secam_lo_trim())
}

// ============================================================================
// Multithreaded decoding
// ============================================================================
//
// Decoding is inherently serial: each field's levels and phase are built up from
// the fields before it. To parallelize we run several decoders concurrently,
// each seeked `distance_size` fields apart, and stitch their outputs back into a
// single stream. A decoder that starts mid-file produces a garbage first field
// while it locks on, then converges onto exactly the output a single serial
// decode would have produced for that part of the file. We exploit that by
// having the earlier ("authoritative") decoder overlap into the next decoder's
// region and, once `overlap_count` consecutive fields agree, hand off to the
// later decoder — which has already decoded ahead in parallel.
//
// Determinism: segment boundaries are fixed (`seg * distance_size` fields), each
// decoder is deterministic, and every stitch decision (earlier decoder wins,
// first `overlap_count`-in-a-row match stitches) is independent of thread timing.
// So the output depends only on the parameters and input, never on scheduling.

/// Multithreading parameters. See the `--mt-*` CLI flags.
pub struct MtParams {
    /// Number of concurrent decoder threads. Must be >= 1 here (the `== 0` case
    /// is handled by the serial [`decode_all`]).
    pub threads: usize,
    /// Fields of distance between each thread's start offset, and the width of
    /// the overlap window searched for a stitch.
    pub distance_size: u64,
    /// Number of consecutive matching fields required to stitch.
    pub overlap_count: usize,
    /// Per-field MSRE threshold below which two fields are considered equal.
    pub threshold: f64,
    /// Fraction of the largest per-sample squared deviations discarded before.
    pub trim_fraction: f64,
}

// --- Field comparison --------------------------------------------------------

use crate::fields_match::{f32_msre, wrapped_u16_msre};

fn luma_matches(a: &LumaOutput, b: &LumaOutput, mt: &MtParams) -> bool {
    match (a, b) {
        (LumaOutput::Encoded(x), LumaOutput::Encoded(y)) => {
            x.len() == y.len() && wrapped_u16_msre(x, y, mt.trim_fraction) < mt.threshold
        }
        (LumaOutput::Raw(x), LumaOutput::Raw(y)) => {
            x.len() == y.len() && f32_msre(x, y, mt.trim_fraction) < mt.threshold
        }
        // Different output kinds can never be stitched together.
        _ => false,
    }
}

/// Whether two fields are equal enough to stitch. With `compare_chroma` off
/// only the luma decides: chroma whose carrier phase continues across fields
/// (see [`DecoderSpec::chroma_phase_continuous`]) never agrees between two
/// decoders, so a stitch there accepts one carrier phase step in the output.
fn fields_match(
    a: &WriteableField,
    b: &WriteableField,
    mt: &MtParams,
    compare_chroma: bool,
) -> bool {
    if a.info.is_first_field != b.info.is_first_field {
        return false;
    }
    if a.info.sync_conf != b.info.sync_conf {
        return false;
    }
    if !luma_matches(a.luma(), b.luma(), mt) {
        return false;
    }
    if !compare_chroma {
        return true;
    }
    match (a.chroma(), b.chroma()) {
        (Some(x), Some(y)) => {
            x.len() == y.len() && wrapped_u16_msre(x, y, mt.trim_fraction) < mt.threshold
        }
        (None, None) => true,
        _ => false,
    }
}

// --- Shared sequential input ------------------------------------------------
//
// Every worker shares one view of the input through `Tape`. A dedicated
// forerunner thread owns the source and is the only thing that reads it: it
// streams the input forward, once, into a shared buffer, staying ahead of the
// furthest sample any decoder has asked for. Workers are pure consumers — they
// take the buffer lock, copy out the samples they need, and, if those samples
// are not buffered yet, park on a condvar until the forerunner delivers them.
// The forerunner reads into a private scratch buffer with no lock held, so the
// read syscall never blocks the decoders, and takes the lock only to append the
// block it just read. Every reader publishes the first sample it still needs,
// and the forerunner drops everything before the least of those (and before the
// start of a segment still to be spawned) as it appends, so the buffered span
// follows the slowest live decoder whatever the orchestrator is waiting on.
// Because the source is only ever read forward, this works on non-seekable
// inputs such as pipes.

/// Largest block the forerunner reads from the source in one syscall.
const TAPE_READ_BLOCK: usize = 1 << 20;

/// How far ahead of the furthest requested sample the forerunner keeps the
/// buffer filled, so decoders find their input already buffered. The buffered
/// span stays bounded because worker demand is bounded (see `refill`).
const TAPE_READ_AHEAD: u64 = 8 << 20;

struct Tape {
    /// Buffered input and its bookkeeping. A plain mutex (rather than an
    /// `RwLock`) suffices now that the read syscall happens off-lock in the
    /// forerunner: every critical section here is a short copy or append.
    state: Mutex<BufState>,
    /// Signalled when the forerunner extends the frontier or reaches EOF; waited
    /// on by workers blocked for samples not yet buffered.
    data_ready: Condvar,
    /// Signalled when a worker raises `demand` (or on shutdown); waited on by the
    /// forerunner once it has read far enough ahead of demand.
    more_demand: Condvar,
    /// The forerunner's join handle, taken and joined by `shutdown`.
    forerunner: Mutex<Option<JoinHandle<()>>>,
}

struct BufState {
    /// Buffered input samples (widened to `f32`) covering `[start, start + buf.len())`.
    buf: Vec<f32>,
    /// Absolute sample offset of `buf[0]`.
    start: u64,
    /// Set once the source has reported end of input or failed.
    eof: bool,
    /// Total input length in samples, known once the source reported EOF.
    len: Option<u64>,
    /// The input error that ended the stream, rendered with its cause chain
    /// so every worker whose read runs into it can report it.
    error: Option<String>,
    /// Tells the forerunner to stop reading and exit. Raised under the lock,
    /// like every stop flag a waiter on the tape's condvars checks: the waiter
    /// checks it with the lock held and parks releasing the lock atomically,
    /// so it either sees the flag before parking or is parked when the notify
    /// that follows the raise arrives.
    stop: bool,
    /// The progress of the live readers: the first sample each still needs,
    /// which only ever advances, and which each reads at or beyond. A counter
    /// whose handles are all dropped belongs to a reader that has finished
    /// (a worker thread holds one until it exits).
    readers: Vec<Weak<AtomicU64>>,
    /// The first sample a reader still to be started will need, while one is
    /// expected: the buffer is kept from there for it.
    reserve: Option<u64>,
    /// Highest absolute offset any worker has requested. The forerunner reads up
    /// to `demand + TAPE_READ_AHEAD`. Reading ahead of actual demand (rather than
    /// to a fixed cap) means it can never park while a worker still waits for
    /// input it chose not to read, so there is no risk of deadlock.
    demand: u64,
}

impl Tape {
    /// Build the tape and spawn its forerunner thread, which owns `source` for
    /// the rest of the decode. `reserve` is the first sample the first reader
    /// will need. The returned handle must be passed to [`Tape::shutdown`]
    /// once decoding finishes so the thread is joined.
    fn new(source: DecodeReader, reserve: u64) -> Arc<Self> {
        let tape = Arc::new(Self {
            state: Mutex::new(BufState {
                buf: Vec::new(),
                start: 0,
                eof: false,
                len: None,
                error: None,
                stop: false,
                readers: Vec::new(),
                reserve: Some(reserve),
                demand: 0,
            }),
            data_ready: Condvar::new(),
            more_demand: Condvar::new(),
            forerunner: Mutex::new(None),
        });
        let runner = Arc::clone(&tape);
        let handle = thread::spawn(move || runner.forerun(source));
        *tape.forerunner.lock().unwrap() = Some(handle);
        tape
    }

    /// Total input length in samples, or `None` until the source hits EOF.
    fn known_len(&self) -> Option<u64> {
        self.state.lock().unwrap().len
    }

    /// Register a reader by its progress counter, before it reads: the tape
    /// keeps every sample from the counter on for as long as a handle to it
    /// exists.
    fn add_reader(&self, progress: &Arc<AtomicU64>) {
        self.state
            .lock()
            .unwrap()
            .readers
            .push(Arc::downgrade(progress));
    }

    /// Set the first sample a reader still to be started will need, or that
    /// none is expected, and drop what that releases.
    fn set_reserve(&self, reserve: Option<u64>) {
        let mut state = self.state.lock().unwrap();
        state.reserve = reserve;
        Self::drop_prefix(&mut state);
    }

    /// Drop the buffered samples no reader will ask for: those before the
    /// least progress of the live readers and before the reserve. Readers
    /// that have finished are forgotten here.
    fn drop_prefix(state: &mut BufState) {
        state.readers.retain(|reader| reader.strong_count() != 0);
        let floor = state
            .readers
            .iter()
            .filter_map(Weak::upgrade)
            .map(|progress| progress.load(Ordering::Relaxed))
            .chain(state.reserve)
            .min();
        let Some(floor) = floor else {
            return;
        };
        let drop_to = floor.min(state.start + state.buf.len() as u64);
        if drop_to > state.start {
            let n = (drop_to - state.start) as usize;
            state.buf.drain(..n);
            state.start = drop_to;
        }
    }

    /// Copy the samples `[from, from + dst.len())` out of `state` into `dst`,
    /// truncated at the buffered frontier. Returns the count written.
    fn copy_out(state: &BufState, from: u64, dst: &mut [f32]) -> usize {
        assert!(
            from >= state.start,
            "read at sample {from} behind the buffered start {}: the drop threshold passed a worker's input",
            state.start
        );
        let lo = ((from - state.start) as usize).min(state.buf.len());
        let hi = lo.saturating_add(dst.len()).min(state.buf.len());
        let got = hi - lo;
        dst[..got].copy_from_slice(&state.buf[lo..hi]);
        got
    }

    /// Fill `dst` with samples starting at absolute offset `from`, blocking until
    /// the forerunner has buffered them (or end of input is reached). Returns the
    /// number of samples written and whether end of input was reached (fewer than
    /// `dst.len()` were available), or `None` if `stop` was raised; a request
    /// reaching past an input error fails with that error. `from` is at or
    /// after the reader's registered progress, which the buffer is kept from.
    fn read_into(
        &self,
        from: u64,
        dst: &mut [f32],
        stop: &AtomicBool,
    ) -> Result<Option<(usize, bool)>> {
        let want = dst.len() as u64;
        let target = from + want;
        let mut state = self.state.lock().unwrap();
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if state.start + state.buf.len() as u64 >= target {
                return Ok(Some((Self::copy_out(&state, from, dst), false)));
            }
            if state.eof {
                if let Some(message) = &state.error {
                    bail!("{message}");
                }
                return Ok(Some((Self::copy_out(&state, from, dst), true)));
            }
            // Not buffered yet: raise the forerunner's read target to cover us,
            // then wait for it to deliver. Because it always reads ahead of
            // `demand`, raising it can never leave us blocked on skipped input.
            if target > state.demand {
                state.demand = target;
                self.more_demand.notify_one();
            }
            state = self.data_ready.wait(state).unwrap();
        }
    }

    /// The forerunner thread body: own the source and stream it forward into the
    /// shared buffer, keeping the frontier `TAPE_READ_AHEAD` ahead of the furthest
    /// requested sample. The read syscall runs with no lock held; the lock is
    /// taken only to append the block (or mark EOF) and wake waiting workers.
    fn forerun(&self, mut source: DecodeReader) {
        let mut scratch = vec![0.0f32; TAPE_READ_BLOCK];
        loop {
            // Decide how much to read, or park until demand grows (or we stop).
            let to_read = {
                let mut state = self.state.lock().unwrap();
                loop {
                    if state.stop || state.eof {
                        return;
                    }
                    let frontier = state.start + state.buf.len() as u64;
                    let target = state.demand.saturating_add(TAPE_READ_AHEAD);
                    if frontier < target {
                        break (target - frontier).min(TAPE_READ_BLOCK as u64) as usize;
                    }
                    state = self.more_demand.wait(state).unwrap();
                }
            };
            // Read the next block with no lock held.
            let read = source.read(&mut scratch[..to_read]);
            let mut state = self.state.lock().unwrap();
            Self::drop_prefix(&mut state);
            match read {
                Ok(0) => {
                    state.eof = true;
                    state.len = Some(state.start + state.buf.len() as u64);
                }
                Ok(n) => state.buf.extend_from_slice(&scratch[..n]),
                // Ends the stream like EOF so waiters wake; `read_into` fails
                // the requests that reach past it.
                Err(error) => {
                    state.error = Some(format!("{error:#}"));
                    state.eof = true;
                }
            }
            self.data_ready.notify_all();
        }
    }

    /// Stop the forerunner thread and join it. Must be called once decoding is
    /// finished (the workers, which hold their own `Arc<Tape>`, are joined first),
    /// so the thread releases its reference and the tape can be dropped.
    fn shutdown(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.stop = true;
            self.more_demand.notify_all();
        }
        let handle = self.forerunner.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

/// The first absolute sample offset a decoder started at `start_offset` will
/// read. A decode call with an empty window does no work and leaves the decoder
/// untouched, but reports exactly this offset — the point a serial decode would
/// skip its input to. Used to start each worker's window without reading or
/// seeking earlier input, and to tell the tape how much it may drop.
fn first_needed_offset(spec: &Arc<DecoderSpec>, start_offset: u64) -> u64 {
    let mut probe = Decoder::new(Arc::clone(spec), start_offset);
    probe.decode(&[], 0, false).0
}

// --- Worker thread ----------------------------------------------------------

enum WorkerMsg {
    Field(WriteableField),
    /// Sent once, as soon as the decoder has locked on far enough to report it, so
    /// the orchestrator can write metadata into the JSON while decoding continues.
    Meta(DecoderMetadata),
    /// Sent once on natural end-of-input: the decoder's final metadata, or an
    /// error if the decode failed. Stopped workers send nothing.
    Done(Result<Option<DecoderMetadata>>),
}

/// Decode forward from `start_offset` to end-of-input, sending each field over
/// `tx`. Input comes from the shared `tape`, read once and only forward, so this
/// works on non-seekable inputs. `start_needed` is the first sample the decoder
/// reads (see [`first_needed_offset`]); feeding the window from there reproduces
/// the single-threaded byte stream without touching earlier input. Returns the
/// final metadata; `Ok(None)` if asked to stop early.
fn decode_segment(
    spec: &Arc<DecoderSpec>,
    tape: &Tape,
    start_offset: u64,
    start_needed: u64,
    tx: &SyncSender<WorkerMsg>,
    stop: &AtomicBool,
    consumed_pos: &AtomicU64,
) -> Result<Option<DecoderMetadata>> {
    let mut decoder = Decoder::new(Arc::clone(spec), start_offset);
    let chunk = spec.readlen() + 4 * BLOCKSIZE;
    let mut window: Vec<f32> = Vec::new();
    let mut read_buffer = vec![0.0f32; chunk];
    let mut base: u64 = start_needed;
    let mut meta_sent = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let frontier = base + window.len() as u64;
        let Some((read, final_chunk)) = tape.read_into(frontier, &mut read_buffer, stop)? else {
            return Ok(None);
        };
        window.extend_from_slice(&read_buffer[..read]);

        let (consumed, fields) = decoder.decode(&window, base, final_chunk);
        if !meta_sent {
            if let Some(metadata) = decoder.metadata() {
                if tx.send(WorkerMsg::Meta(metadata)).is_err() {
                    return Ok(None);
                }
                meta_sent = true;
            }
        }
        for field in fields {
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if tx.send(WorkerMsg::Field(field)).is_err() {
                // The orchestrator dropped our receiver: it no longer wants us.
                return Ok(None);
            }
        }

        if final_chunk {
            break;
        }
        if consumed > base + window.len() as u64 {
            // The decoder skipped past everything buffered; resume at `consumed`.
            window.clear();
        } else {
            window.drain(..(consumed - base) as usize);
        }
        base = consumed;
        // Publish how far this decoder has advanced; the tape drops the input
        // before the slowest live decoder's progress.
        consumed_pos.store(base, Ordering::Relaxed);
    }

    Ok(decoder.metadata())
}

/// The error for a worker thread's panic payload, carrying its message.
fn panic_error(payload: Box<dyn Any + Send>) -> anyhow::Error {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    anyhow!("decoder thread panicked: {message}")
}

/// Orchestrator-side handle for one worker thread, with a small look-ahead
/// buffer so fields can be peeked (for `file_loc` alignment) before being taken.
struct Worker {
    rx: Receiver<WorkerMsg>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    buf: VecDeque<WriteableField>,
    finished: bool,
    produced_any: bool,
    outcome: Option<Result<Option<DecoderMetadata>>>,
    /// This worker's decoder metadata, once it has reported it (see
    /// [`WorkerMsg::Meta`]); used to keep the JSON sidecar's metadata populated.
    metadata: Option<DecoderMetadata>,
    /// The decoder's current `consumed` offset (the first sample it still needs),
    /// published each decode step and registered with the tape, which keeps
    /// its input from the least such offset of the live workers; the
    /// orchestrator reads it to throttle the look-ahead.
    consumed: Arc<AtomicU64>,
    /// Shared tape, kept so [`Worker::shutdown`] can wake the worker if it is
    /// parked waiting for samples that will never come.
    tape: Arc<Tape>,
}

impl Worker {
    fn spawn(
        spec: &Arc<DecoderSpec>,
        tape: &Arc<Tape>,
        start_offset: u64,
        start_needed: u64,
        capacity: usize,
    ) -> Self {
        let (tx, rx) = sync_channel(capacity);
        let stop = Arc::new(AtomicBool::new(false));
        let consumed = Arc::new(AtomicU64::new(start_needed));
        tape.add_reader(&consumed);
        let spec = Arc::clone(spec);
        let worker_tape = Arc::clone(tape);
        let tape = Arc::clone(tape);
        let worker_stop = Arc::clone(&stop);
        let worker_consumed = Arc::clone(&consumed);
        let handle = thread::spawn(move || {
            let outcome = decode_segment(
                &spec,
                &tape,
                start_offset,
                start_needed,
                &tx,
                &worker_stop,
                &worker_consumed,
            );
            // If we were stopped, the receiver is gone and the result is moot.
            if !worker_stop.load(Ordering::Relaxed) {
                let _ = tx.send(WorkerMsg::Done(outcome));
            }
        });
        Self {
            rx,
            stop,
            handle: Some(handle),
            buf: VecDeque::new(),
            finished: false,
            produced_any: false,
            outcome: None,
            metadata: None,
            consumed,
            tape: worker_tape,
        }
    }

    /// Ensure at least one field is buffered, blocking on the worker if needed.
    /// Returns false once the worker has finished with nothing more to give.
    fn fill(&mut self) -> bool {
        if !self.buf.is_empty() {
            return true;
        }
        if self.finished {
            return false;
        }
        loop {
            match self.rx.recv() {
                Ok(WorkerMsg::Field(field)) => {
                    self.produced_any = true;
                    self.buf.push_back(field);
                    return true;
                }
                // Metadata arrives out of band; record it and keep waiting for the
                // field this call promised to buffer.
                Ok(WorkerMsg::Meta(metadata)) => {
                    self.metadata = Some(metadata);
                }
                Ok(WorkerMsg::Done(result)) => {
                    self.outcome = Some(result);
                    self.finished = true;
                    return false;
                }
                // Only a panic closes the channel without a Done: a stopped
                // worker is never polled, and every other exit sends one.
                Err(_) => {
                    let handle = self
                        .handle
                        .take()
                        .expect("the worker thread is joined once");
                    let payload = handle.join().expect_err(
                        "a worker that is not stopped sends its outcome before exiting",
                    );
                    self.outcome = Some(Err(panic_error(payload)));
                    self.finished = true;
                    return false;
                }
            }
        }
    }

    fn peek(&mut self) -> Option<&WriteableField> {
        if self.fill() {
            self.buf.front()
        } else {
            None
        }
    }

    fn pop(&mut self) -> Option<WriteableField> {
        if self.fill() {
            self.buf.pop_front()
        } else {
            None
        }
    }

    /// Signal the worker to stop and join it. Dropping the receiver also unblocks
    /// a worker parked on a full channel. A panic, or a failed outcome nothing
    /// absorbed, is the error.
    fn shutdown(mut self) -> Result<()> {
        // Raised under the tape lock and followed by the wake-up, so a worker
        // checking the flag before parking in the tape sees it, and one already
        // parked is woken (see `BufState::stop`); the dropped receiver below
        // only unblocks one parked on a full send.
        {
            let _state = self.tape.state.lock().unwrap();
            self.stop.store(true, Ordering::Relaxed);
            self.tape.data_ready.notify_all();
        }
        let handle = self.handle.take();
        // Dropping the receiver closes the channel, unblocking a worker parked on
        // a full send so the join below cannot hang.
        drop(self.rx);
        if let Some(handle) = handle {
            handle.join().map_err(panic_error)?;
        }
        match self.outcome.take() {
            Some(Err(error)) => Err(error),
            _ => Ok(()),
        }
    }
}

// --- Orchestrator -----------------------------------------------------------

/// Stitches the output of several [`Worker`]s into a single in-order field
/// stream written through `writer`.
struct MtOrchestrator<'a> {
    writer: &'a mut DecodeWriter,
    spec: Arc<DecoderSpec>,
    /// Shared single-pass view of the input, streamed once to all workers.
    tape: Arc<Tape>,
    mt: MtParams,
    /// Absolute sample offset where decoding begins (`--offset`).
    global_base: u64,
    /// Samples per field, for translating field distances into seek offsets.
    spf: u64,
    capacity: usize,
    /// Active workers keyed by segment index.
    pool: HashMap<u64, Worker>,
    /// Lowest segment index not yet assigned to a worker.
    next_unassigned: u64,
    /// The first sample `next_unassigned`'s worker will read (see
    /// [`first_needed_offset`]), published to the tape as its reserve while
    /// that segment may still be spawned.
    next_unassigned_needed: u64,
    /// Fields committed to the output so far (the running global sequence count).
    committed: usize,
    /// Decoder metadata to embed in the JSON sidecar while decoding runs, taken
    /// from the first worker to report it (it is the same for every worker). The
    /// final, authoritative metadata is written from `final_metadata` at close.
    metadata: Option<DecoderMetadata>,
    final_metadata: Option<DecoderMetadata>,
}

impl<'a> MtOrchestrator<'a> {
    /// Absolute start sample offset of a segment.
    fn seg_start(&self, seg: u64) -> u64 {
        self.global_base
            + seg
                .saturating_mul(self.mt.distance_size)
                .saturating_mul(self.spf)
    }

    /// Keep up to `threads` workers decoding ahead, spawning the lowest
    /// unassigned segments. The input length is unknown until the source hits
    /// EOF; once known, segments past it are skipped (a worker started past EOF
    /// would simply produce nothing, so this is only an optimization).
    ///
    /// Speculative workers beyond `next_seg` are throttled: one is spawned only
    /// while its start stays within a budget ahead of the slowest live
    /// decoder. This stops the look-ahead cohort from lapping a pinned
    /// authoritative thread when stitches keep failing, which would otherwise
    /// let the buffered span grow without bound. The current and next
    /// segments are always spawnable, so stitching can always make progress.
    ///
    /// The tape's reserve follows: the first sample the next unassigned
    /// segment's worker will read while one may still be spawned, none once
    /// the known input length ends before it.
    fn refill(&mut self, next_seg: u64) -> Result<()> {
        // One full cohort of segments ahead of the slowest decoder.
        let budget = (self.mt.threads as u64)
            .saturating_mul(self.mt.distance_size)
            .saturating_mul(self.spf);
        let past_input = |orchestrator: &Self, seg: u64| {
            orchestrator
                .tape
                .known_len()
                .is_some_and(|len| orchestrator.seg_start(seg) >= len)
        };
        while self.pool.len() < self.mt.threads {
            let seg = self.next_unassigned;
            if past_input(self, seg) {
                break;
            }
            if seg > next_seg
                && self
                    .slowest_progress()
                    .is_some_and(|progress| self.seg_start(seg) > progress.saturating_add(budget))
            {
                break;
            }
            let worker = Worker::spawn(
                &self.spec,
                &self.tape,
                self.seg_start(seg),
                self.next_unassigned_needed,
                self.capacity,
            );
            self.pool.insert(seg, worker);
            self.next_unassigned += 1;
            self.next_unassigned_needed =
                first_needed_offset(&self.spec, self.seg_start(self.next_unassigned));
        }
        let reserve =
            (!past_input(self, self.next_unassigned)).then_some(self.next_unassigned_needed);
        self.tape.set_reserve(reserve);
        Ok(())
    }

    /// The first sample the slowest live decoder still needs.
    fn slowest_progress(&self) -> Option<u64> {
        self.pool
            .values()
            .map(|worker| worker.consumed.load(Ordering::Relaxed))
            .min()
    }

    /// Take a finished worker's outcome: surface a decode error, or capture the
    /// metadata of the decoder that reached this part of the file.
    fn absorb_outcome(&mut self, seg: u64) -> Result<()> {
        let outcome = self.pool.get_mut(&seg).and_then(|w| w.outcome.take());
        match outcome {
            Some(Ok(Some(metadata))) => self.final_metadata = Some(metadata),
            Some(Ok(None)) | None => {}
            Some(Err(error)) => return Err(error),
        }
        Ok(())
    }

    /// Renumber a field's sequence-derived metadata against its global position
    /// and write it out.
    fn commit(&mut self, mut field: WriteableField) -> Result<()> {
        field.set_sequence_position(self.committed + 1);
        if self.metadata.is_none() {
            self.metadata = self
                .pool
                .values()
                .find_map(|worker| worker.metadata.clone());
        }
        self.writer
            .write_writeable(&field, self.metadata.as_ref())?;
        self.committed += 1;
        Ok(())
    }

    fn shutdown_worker(&mut self, seg: u64) -> Result<()> {
        match self.pool.remove(&seg) {
            Some(worker) => worker.shutdown(),
            None => Ok(()),
        }
    }

    /// Advance `next`'s buffer to the field aligned with `file_loc` and report
    /// whether it matches `current_field`. The aligned field is consumed so
    /// the later decoder, if it takes over, resumes immediately after it.
    fn compare_with_next(&mut self, next_seg: u64, current_field: &WriteableField) -> bool {
        let tol = self.spf / 2;
        let file_loc = current_field.info.file_loc;
        let aligned = {
            let Some(worker) = self.pool.get_mut(&next_seg) else {
                return false;
            };
            // Drop any of `next`'s fields that fall before this one (extra fields
            // it produced while still locking on).
            while worker
                .peek()
                .is_some_and(|g| g.info.file_loc + tol < file_loc)
            {
                worker.pop();
            }
            match worker.peek() {
                Some(g) if g.info.file_loc.abs_diff(file_loc) <= tol => worker.pop().unwrap(),
                // `next` has no field at this location yet (or is finished).
                _ => return false,
            }
        };
        fields_match(
            current_field,
            &aligned,
            &self.mt,
            !self.spec.chroma_phase_continuous(),
        )
    }

    /// Run the decode, always tearing down workers before returning (even on
    /// error), and writing the output only on success.
    fn run(&mut self) -> Result<()> {
        let result = self.decode_loop();
        // Tear down every still-running worker regardless of outcome; a
        // failure found there fails a decode that otherwise succeeded.
        let mut teardown = Ok(());
        for (_, worker) in std::mem::take(&mut self.pool) {
            teardown = teardown.and(worker.shutdown());
        }
        result?;
        teardown?;
        self.write_output()
    }

    /// The stitching loop. Commits fields in order, leaving `final_metadata` set
    /// to the metadata of whichever decoder reached end of input.
    fn decode_loop(&mut self) -> Result<()> {
        let mut current_seg = 0u64;
        let mut next_seg = 1u64;
        self.refill(next_seg)?;
        if !self.pool.contains_key(&current_seg) {
            // Nothing decodable (input shorter than the start offset).
            return Ok(());
        }

        loop {
            let region_start = self.seg_start(next_seg);
            let region_end = self.seg_start(next_seg + 1);

            // No further segment will be spawned (the next one lies past the
            // input, or the pool has no room for it until the current decoder
            // finishes): the current decoder is the final authority; drain it
            // to end of input, with the tape following it alone.
            if !self.pool.contains_key(&next_seg) {
                self.tape.set_reserve(None);
                while let Some(field) = self.pool.get_mut(&current_seg).unwrap().pop() {
                    self.commit(field)?;
                }
                self.absorb_outcome(current_seg)?;
                return Ok(());
            }

            // Phase 1: commit the current decoder's fields up to the next
            // decoder's region.
            loop {
                let reached = match self.pool.get_mut(&current_seg).unwrap().peek() {
                    Some(field) => field.info.file_loc >= region_start,
                    // Current reached end of input before the next region; it is
                    // the final authority.
                    None => {
                        self.absorb_outcome(current_seg)?;
                        return Ok(());
                    }
                };
                if reached {
                    break;
                }
                let field = self.pool.get_mut(&current_seg).unwrap().pop().unwrap();
                self.commit(field)?;
            }

            // Phase 2: in the overlap window, commit the current decoder's fields
            // while comparing against the next decoder, until `overlap_count`
            // consecutive fields match (stitch) or the window is exhausted.
            let mut run = 0usize;
            let mut stitched = false;
            loop {
                let in_window = match self.pool.get_mut(&current_seg).unwrap().peek() {
                    Some(field) => field.info.file_loc < region_end,
                    None => {
                        self.absorb_outcome(current_seg)?;
                        return Ok(());
                    }
                };
                if !in_window {
                    break;
                }
                let field = self.pool.get_mut(&current_seg).unwrap().pop().unwrap();
                let matched = self.compare_with_next(next_seg, &field);
                self.commit(field)?;
                if matched {
                    run += 1;
                    if run >= self.mt.overlap_count {
                        stitched = true;
                        break;
                    }
                } else {
                    run = 0;
                }
            }

            if stitched {
                // The next decoder, already past the matched fields, takes over.
                self.shutdown_worker(current_seg)?;
                current_seg = next_seg;
                next_seg += 1;
            } else {
                // The whole overlap disagreed: the earlier decoder stays
                // authoritative and the later decoder's work is thrown away.
                let produced = self.pool.get(&next_seg).is_some_and(|w| w.produced_any);
                let start = self.seg_start(next_seg);
                self.shutdown_worker(next_seg)?;
                if produced {
                    tracing::warn!(
                        segment_start_sample = start,
                        "Discarded a thread's work: no {}-field match found within {} overlapping fields; the earlier thread remains authoritative",
                        self.mt.overlap_count,
                        self.mt.distance_size,
                    );
                }
                next_seg += 1;
            }
            self.refill(next_seg)?;
        }
    }

    fn write_output(&mut self) -> Result<()> {
        if self.committed != 0 {
            tracing::info!("Completed: saving JSON and exiting.");
        } else {
            tracing::info!("Completed without handling any frames.");
        }
        self.writer.close(self.final_metadata.take())?;
        Ok(())
    }
}

/// Multithreaded counterpart to [`decode_all`]. Requires `mt.threads >= 1`;
/// `mt.threads == 0` is handled by the serial path. `start_offset` is the
/// absolute sample where decoding begins (`--offset`, 0 by default).
///
/// The input is streamed once, forward, by the [`Tape`]'s forerunner thread;
/// the workers never read, reopen, seek, or stat it, so this runs on
/// non-seekable inputs such as pipes. The decoders themselves skip past the
/// input before `start_offset`, so reading still begins at the stream's start.
pub fn decode_all_mt(
    reader: DecodeReader,
    writer: &mut DecodeWriter,
    spec: Arc<DecoderSpec>,
    mt: MtParams,
    start_offset: u64,
) -> Result<()> {
    let next_unassigned_needed = first_needed_offset(&spec, start_offset);
    let tape = Tape::new(reader, next_unassigned_needed);
    let spf = spec.samples_per_field();
    // A worker only needs to bank its own segment plus the handful of fields it
    // overlaps into the next segment to hand off (a one-field warm-up plus
    // `overlap_count` matches). Anything decoded beyond that is thrown away when
    // the worker is stitched out, so the buffer is kept tight to avoid wasted
    // decode work; a small margin keeps the hand-off from stalling on a live
    // decode. The cap bounds memory when --mt-distance-size is large; correctness
    // is unaffected, as workers stream further fields on demand. It also caps how
    // far ahead a worker reads the shared tape, bounding the tape's buffered span.
    const HANDOFF_MARGIN: usize = 3;
    const MAX_BUFFERED_FIELDS: usize = 256;
    let capacity = (mt.distance_size as usize)
        .saturating_add(mt.overlap_count)
        .saturating_add(HANDOFF_MARGIN)
        .clamp(4, MAX_BUFFERED_FIELDS);

    let mut orchestrator = MtOrchestrator {
        writer,
        spec,
        tape: Arc::clone(&tape),
        mt,
        global_base: start_offset,
        spf,
        capacity,
        pool: HashMap::new(),
        next_unassigned: 0,
        next_unassigned_needed,
        committed: 0,
        metadata: None,
        final_metadata: None,
    };
    // Tear down the forerunner thread whether the decode succeeded or failed
    // (the orchestrator has already joined every worker by the time it returns).
    let result = orchestrator.run();
    tape.shutdown();
    result
}
