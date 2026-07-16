//! The timeline audio engine: mixes every audio-contributing clip at the
//! playhead, sample-accurately, and owns the master [`PlaybackClock`].
//!
//! Pipeline: a render thread pulls per-clip samples through
//! [`ClipReader`]s (decode → mixdown to stereo → linear resample →
//! per-sample gain: volume × keyframe envelope × transition ramps),
//! sums them into fixed blocks through the master soft-clamp
//! ([`soft_clip`]), and pushes the blocks into an SPSC ring; the cpal
//! callback pops the ring and advances the clock.
//! Timeline gaps render as silence, so while playing the clock always
//! advances — playback continues through gaps by construction.
//!
//! Sample accuracy: segment in/out points are rounded once to output-rate
//! frame indices, and every render block is split at those boundaries, so
//! a cut lands on an exact sample regardless of block phase. Two clips
//! that touch (`a.out == b.in`) produce gapless, overlap-free audio.
//!
//! Realtime rules (inherited from Phase 0): the cpal callback never locks,
//! decodes, or allocates — it pops the ring, feeds silence while paused,
//! and applies pending seek rebases by draining stale samples.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

use crate::clock::PlaybackClock;
use crate::error::AudioError;
use crate::source::{AudioSource, SymphoniaSource};

/// Ring buffer capacity in seconds. Small enough that a post-seek refill
/// is instant, big enough to ride out scheduling hiccups.
const RING_SECONDS: f64 = 0.25;

/// Frames rendered per block. Blocks are also the command-response
/// granularity of the render thread (~11 ms at 48 kHz).
pub(crate) const BLOCK_FRAMES: usize = 512;

/// Poll cadence while the ring is full / a rebase is pending.
const IDLE_POLL: Duration = Duration::from_millis(3);

/// Easing of a volume-envelope segment. Mirrors `cutty_engine::Easing`
/// one-to-one — the mixer stays engine-independent (like the rest of
/// [`AudioSegment`]), so the playback layer maps between the two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Easing {
    #[default]
    Linear,
    /// `x²`.
    EaseIn,
    /// `x·(2−x)`.
    EaseOut,
    /// Smoothstep `x²·(3−2x)`.
    EaseInOut,
}

impl Easing {
    fn apply(self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Easing::Linear => x,
            Easing::EaseIn => x * x,
            Easing::EaseOut => x * (2.0 - x),
            Easing::EaseInOut => x * x * (3.0 - 2.0 * x),
        }
    }
}

/// One point of a clip's volume automation, **timeline** seconds (the
/// playback layer bakes clip-relative keyframe times into absolute ones
/// when it builds segments). `easing` shapes the segment to the next
/// point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvelopePoint {
    pub t: f64,
    pub value: f64,
    pub easing: Easing,
}

/// A clip's volume envelope: a gain **multiplier on top of the
/// segment's static `volume`**, evaluated per sample. Constant at the
/// first/last point's value outside the point range, eased in between —
/// exactly the engine's keyframe evaluation semantics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VolumeEnvelope {
    /// Sorted by `t`, non-empty in practice (an empty envelope is
    /// represented as `AudioSegment::envelope: None`).
    pub points: Vec<EnvelopePoint>,
}

impl VolumeEnvelope {
    /// The gain multiplier at timeline time `t`.
    pub fn gain_at(&self, t: f64) -> f64 {
        let Some(last) = self.points.last() else {
            return 1.0;
        };
        let i = self.points.partition_point(|p| p.t <= t);
        if i == 0 {
            return self.points[0].value;
        }
        if i == self.points.len() {
            return last.value;
        }
        let a = &self.points[i - 1];
        let b = &self.points[i];
        let span = b.t - a.t;
        if span <= 0.0 {
            return b.value;
        }
        a.value + (b.value - a.value) * a.easing.apply((t - a.t) / span)
    }
}

/// An equal-power fade over `[start, end]` timeline seconds — the audio
/// half of a video transition. The incoming gain rises `sin(x·π/2)`, the
/// outgoing falls `cos(x·π/2)`, so a crossfading pair sums to constant
/// power. Degenerate ramps (`end <= start`) read as unity gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FadeRamp {
    pub start: f64,
    pub end: f64,
}

impl FadeRamp {
    fn x(&self, t: f64) -> f64 {
        if self.end <= self.start {
            return 1.0;
        }
        ((t - self.start) / (self.end - self.start)).clamp(0.0, 1.0)
    }

    /// Rising equal-power gain (the incoming clip).
    pub fn gain_in(&self, t: f64) -> f32 {
        (self.x(t) * std::f64::consts::FRAC_PI_2).sin() as f32
    }

    /// Falling equal-power gain (the outgoing clip).
    pub fn gain_out(&self, t: f64) -> f32 {
        (self.x(t) * std::f64::consts::FRAC_PI_2).cos() as f32
    }
}

/// One audio-contributing clip, flattened for the mixer. Built by the
/// playback layer from the project snapshot (the mixer knows nothing
/// about tracks or media ids).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSegment {
    /// File to decode — the proxy for video media, the original for
    /// audio-only media.
    pub path: PathBuf,
    /// Timeline placement, seconds.
    pub timeline_in: f64,
    pub timeline_out: f64,
    /// Start of the used range within the source, seconds.
    pub source_in: f64,
    /// Playback rate (1.0 until Phase 3).
    pub speed: f64,
    /// Linear gain.
    pub volume: f64,
    /// Volume-keyframe automation, a per-sample gain multiplier on top
    /// of `volume` (None = unity).
    pub envelope: Option<VolumeEnvelope>,
    /// Equal-power ramps from video transitions (evaluated per sample on
    /// top of `volume`). Both may be present on a clip inside a chain of
    /// transitions.
    pub fade_in: Option<FadeRamp>,
    pub fade_out: Option<FadeRamp>,
}

/// The mixer's whole input: every audio-contributing clip on the timeline.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MixerTimeline {
    pub segments: Vec<AudioSegment>,
}

/// A segment with its boundaries rounded to output frames. Rounding once
/// keeps touching clips exactly gapless.
pub(crate) struct PlacedSegment {
    seg: AudioSegment,
    start_frame: i64,
    end_frame: i64,
}

pub(crate) fn place(timeline: MixerTimeline, out_rate: u32) -> Vec<PlacedSegment> {
    let rate = f64::from(out_rate);
    timeline
        .segments
        .into_iter()
        .filter_map(|seg| {
            if !(seg.timeline_in.is_finite() && seg.timeline_out.is_finite()) {
                return None;
            }
            let start_frame = (seg.timeline_in * rate).round() as i64;
            let end_frame = (seg.timeline_out * rate).round() as i64;
            (end_frame > start_frame).then_some(PlacedSegment {
                seg,
                start_frame,
                end_frame,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------
// ClipReader: one segment's sample stream, resampled and mixed down
// ---------------------------------------------------------------------

/// Pulls source samples for one segment and accumulates them into output
/// blocks: mixdown to stereo, linear resample to the output rate, scale by
/// clip volume. Past source EOF it contributes silence.
pub(crate) struct ClipReader {
    source: Box<dyn AudioSource>,
    src_rate: f64,
    src_channels: usize,
    /// Stereo frames buffered from the source.
    buf: Vec<f32>,
    /// Absolute source frame index of `buf[0..2]`.
    buf_start: i64,
    /// Fractional source frame position of the next output sample.
    cursor: f64,
    /// The timeline frame `mix_into` expects next; a mismatch repositions.
    expected_frame: Option<i64>,
    /// Source exhausted — remaining frames read as silence.
    eof: bool,
    /// Scratch for source reads (interleaved at source layout).
    scratch: Vec<f32>,
}

impl ClipReader {
    fn new(source: Box<dyn AudioSource>) -> Self {
        let src_rate = f64::from(source.sample_rate().max(1));
        let src_channels = source.channels().max(1);
        Self {
            source,
            src_rate,
            src_channels,
            buf: Vec::new(),
            buf_start: 0,
            cursor: 0.0,
            expected_frame: None,
            eof: false,
            scratch: vec![0.0; 4096 - (4096 % src_channels.max(1))],
        }
    }

    fn buf_frames(&self) -> i64 {
        (self.buf.len() / 2) as i64
    }

    /// Stereo sample at absolute source frame `i` (0.0 outside the buffer:
    /// past EOF, or before a seek landing — both mean silence).
    fn frame_at(&self, i: i64) -> (f32, f32) {
        let rel = i - self.buf_start;
        if rel < 0 || rel >= self.buf_frames() {
            return (0.0, 0.0);
        }
        let k = rel as usize * 2;
        (self.buf[k], self.buf[k + 1])
    }

    /// Pull from the source until the buffer covers frame `up_to` (or EOF).
    fn ensure(&mut self, up_to: i64) -> Result<(), AudioError> {
        while !self.eof && self.buf_start + self.buf_frames() <= up_to {
            let n = self.source.read(&mut self.scratch)?;
            if n == 0 {
                self.eof = true;
                break;
            }
            // Mix down to stereo: mono duplicates, extra channels drop.
            match self.src_channels {
                1 => {
                    for &s in &self.scratch[..n] {
                        self.buf.push(s);
                        self.buf.push(s);
                    }
                }
                2 => self.buf.extend_from_slice(&self.scratch[..n]),
                c => {
                    for frame in self.scratch[..n].chunks_exact(c) {
                        self.buf.push(frame[0]);
                        self.buf.push(frame[1]);
                    }
                }
            }
        }
        Ok(())
    }

    /// Drop buffered frames the cursor has permanently passed.
    fn evict(&mut self) {
        let keep_from = self.cursor.floor() as i64;
        let drop_frames = (keep_from - self.buf_start).clamp(0, self.buf_frames());
        if drop_frames > 0 {
            self.buf.drain(..drop_frames as usize * 2);
            self.buf_start += drop_frames;
        }
    }

    /// Accumulate `out.len()/2` output frames starting at timeline frame
    /// `t0` into `out` (+=, stereo interleaved).
    fn mix_into(
        &mut self,
        seg: &AudioSegment,
        t0: i64,
        out_rate: u32,
        out: &mut [f32],
    ) -> Result<(), AudioError> {
        let rate = f64::from(out_rate);
        if self.expected_frame != Some(t0) {
            // Reposition: cursor to the exact fractional source frame for
            // t0, source to the frame at/below it.
            let t0_secs = t0 as f64 / rate;
            let src_pos = (seg.source_in + (t0_secs - seg.timeline_in) * seg.speed) * self.src_rate;
            let src_pos = src_pos.max(0.0);
            let base = src_pos.floor();
            self.source.seek(base / self.src_rate)?;
            self.buf.clear();
            self.buf_start = base as i64;
            self.cursor = src_pos;
            self.eof = false;
        }

        let step = seg.speed * self.src_rate / rate;
        let base_vol = seg.volume as f32;
        // Per-sample gain: volume automation and transition ramps are
        // both evaluated at every output sample — gain changes are
        // smooth by construction (no block-rate zipper steps).
        let per_sample_gain =
            seg.envelope.is_some() || seg.fade_in.is_some() || seg.fade_out.is_some();
        self.evict();
        for (k, frame) in out.chunks_exact_mut(2).enumerate() {
            let i = self.cursor.floor() as i64;
            self.ensure(i + 1)?;
            let (l0, r0) = self.frame_at(i);
            let (l1, r1) = self.frame_at(i + 1);
            let frac = (self.cursor - i as f64) as f32;
            let mut vol = base_vol;
            if per_sample_gain {
                let t = (t0 + k as i64) as f64 / rate;
                if let Some(envelope) = &seg.envelope {
                    vol *= envelope.gain_at(t) as f32;
                }
                if let Some(ramp) = &seg.fade_in {
                    vol *= ramp.gain_in(t);
                }
                if let Some(ramp) = &seg.fade_out {
                    vol *= ramp.gain_out(t);
                }
            }
            frame[0] += (l0 + (l1 - l0) * frac) * vol;
            frame[1] += (r0 + (r1 - r0) * frac) * vol;
            self.cursor += step;
        }
        self.expected_frame = Some(t0 + (out.len() / 2) as i64);
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Block rendering
// ---------------------------------------------------------------------

/// Factory used by the renderer to open a segment's source (injectable
/// for tests).
pub(crate) type OpenSource<'a> =
    &'a mut dyn FnMut(&AudioSegment) -> Result<Box<dyn AudioSource>, AudioError>;

/// Render one block starting at timeline frame `head` into `out`
/// (stereo interleaved, zeroed and summed here). The block is split at
/// every segment boundary inside it, so cuts are sample-exact.
///
/// `readers` caches open readers by segment index; `None` marks a segment
/// that failed to open/decode (renders as silence, reported once via
/// `on_error`).
pub(crate) fn render_block(
    segments: &[PlacedSegment],
    readers: &mut HashMap<usize, Option<ClipReader>>,
    open: OpenSource<'_>,
    on_error: &mut dyn FnMut(&AudioSegment, String),
    head: i64,
    out_rate: u32,
    out: &mut [f32],
) {
    out.fill(0.0);
    let frames = (out.len() / 2) as i64;
    let block_end = head + frames;

    // Split points: every segment edge that falls inside the block.
    let mut cuts: Vec<i64> = vec![head, block_end];
    for placed in segments {
        for edge in [placed.start_frame, placed.end_frame] {
            if head < edge && edge < block_end {
                cuts.push(edge);
            }
        }
    }
    cuts.sort_unstable();
    cuts.dedup();

    for span in cuts.windows(2) {
        let (s0, s1) = (span[0], span[1]);
        let slice = &mut out[(s0 - head) as usize * 2..(s1 - head) as usize * 2];
        for (idx, placed) in segments.iter().enumerate() {
            if !(placed.start_frame <= s0 && s0 < placed.end_frame) {
                continue;
            }
            let entry = readers
                .entry(idx)
                .or_insert_with(|| match open(&placed.seg) {
                    Ok(source) => Some(ClipReader::new(source)),
                    Err(e) => {
                        on_error(&placed.seg, e.to_string());
                        None
                    }
                });
            if let Some(reader) = entry {
                if let Err(e) = reader.mix_into(&placed.seg, s0, out_rate, slice) {
                    on_error(&placed.seg, e.to_string());
                    *entry = None; // silence from here on
                }
            }
        }
    }

    // Master headroom policy: sum, then soft-clamp. Below the knee the
    // bus is bit-transparent; above it a tanh knee compresses smoothly
    // toward ±1.0, so stacked tracks overload musically instead of
    // hard-clipping (and wrap-around is impossible). One shared mix
    // path (this function) applies it to preview and export alike.
    for s in out.iter_mut() {
        *s = soft_clip(*s);
    }
}

/// Where the master soft-clip knee starts (≈ −1.16 dBFS). Everything
/// below passes through untouched — a legal single-clip mix is
/// bit-identical to the pre-knee sum.
pub const SOFT_CLIP_KNEE: f32 = 0.875;

/// Piecewise soft clip: identity inside `±SOFT_CLIP_KNEE`, then a tanh
/// segment saturating asymptotically at ±1.0. C¹-continuous at the knee
/// (tanh′(0) = 1).
#[inline]
pub fn soft_clip(s: f32) -> f32 {
    let a = s.abs();
    if a <= SOFT_CLIP_KNEE {
        s
    } else {
        let over = (a - SOFT_CLIP_KNEE) / (1.0 - SOFT_CLIP_KNEE);
        (SOFT_CLIP_KNEE + (1.0 - SOFT_CLIP_KNEE) * over.tanh()).copysign(s)
    }
}

/// Drop readers for segments far outside the render window so decoders
/// (and their file handles) don't accumulate.
fn evict_readers(
    segments: &[PlacedSegment],
    readers: &mut HashMap<usize, Option<ClipReader>>,
    head: i64,
    out_rate: u32,
) {
    let horizon = 2 * i64::from(out_rate); // ±2 s
    readers.retain(|&idx, _| {
        segments
            .get(idx)
            .is_some_and(|p| p.end_frame > head - horizon && p.start_frame < head + horizon)
    });
}

// ---------------------------------------------------------------------
// The engine: device stream + render thread
// ---------------------------------------------------------------------

enum MixerCmd {
    SetTimeline(MixerTimeline),
    Seek(f64),
    Stop,
}

/// Timeline audio playback and the master clock.
///
/// `play`/`pause` act instantly (the callback flips to silence without
/// consuming); `seek` and `set_timeline` are applied by the render thread
/// through the ring-drain rebase handshake, keeping position and audible
/// content in lockstep.
pub struct TimelineAudio {
    clock: Arc<PlaybackClock>,
    cmd_tx: Sender<MixerCmd>,
    render_thread: Option<JoinHandle<()>>,
    /// Keeps the device stream alive; dropping it stops audio.
    _stream: cpal::Stream,
    out_rate: u32,
}

/// Factory opening a file as an [`AudioSource`], injectable at the
/// public mixer/offline boundaries so `cutty-media` can supply the
/// symphonia→libav fallback chain (this crate stays libav-free).
pub type SourceFactory = Box<dyn FnMut(&Path) -> Result<Box<dyn AudioSource>, AudioError> + Send>;

/// The default factory: symphonia only.
pub fn symphonia_factory() -> SourceFactory {
    Box::new(|path| Ok(Box::new(SymphoniaSource::open(path)?)))
}

impl TimelineAudio {
    /// Open the default output device and start the (empty, paused)
    /// mixer. `on_error` receives non-fatal per-clip decode problems.
    /// Sources decode through symphonia; use
    /// [`TimelineAudio::open_with_factory`] to inject a codec fallback.
    pub fn open(on_error: Box<dyn Fn(String) + Send>) -> Result<Self, AudioError> {
        Self::open_with_factory(on_error, symphonia_factory())
    }

    /// [`TimelineAudio::open`] with an injectable source factory.
    pub fn open_with_factory(
        on_error: Box<dyn Fn(String) + Send>,
        source_factory: SourceFactory,
    ) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(AudioError::NoDevice)?;
        let config = device.default_output_config()?;
        if config.sample_format() != cpal::SampleFormat::F32 {
            return Err(AudioError::UnsupportedDevice(format!(
                "default output format is {:?}, need f32",
                config.sample_format()
            )));
        }
        let out_rate = config.sample_rate();
        let device_channels = usize::from(config.channels()).max(1);

        let clock = Arc::new(PlaybackClock::new(out_rate));
        let ring_cap = ((f64::from(out_rate) * RING_SECONDS) as usize).max(BLOCK_FRAMES * 2) * 2;
        let (producer, mut consumer) = HeapRb::<f32>::new(ring_cap).split();

        // Preallocated stereo scratch: the callback pops into this, then
        // maps to the device layout. Sized for a generous callback.
        let mut pop_buf = vec![0f32; 8192];

        let cb_clock = clock.clone();
        let stream = device.build_output_stream(
            cpal::StreamConfig {
                channels: device_channels as cpal::ChannelCount,
                sample_rate: out_rate,
                buffer_size: cpal::BufferSize::Default,
            },
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                // 1) Apply a pending seek: drop stale audio and rebase,
                //    even while paused.
                if let Some(base) = cb_clock.take_rebase() {
                    consumer.clear();
                    cb_clock.apply_rebase(base);
                }

                // 2) Feed the device. Paused ⇒ silence without consuming.
                data.fill(0.0);
                let mut media_frames = 0u64;
                if cb_clock.is_playing() {
                    let device_frames = data.len() / device_channels;
                    let want = (device_frames * 2).min(pop_buf.len());
                    let got = consumer.pop_slice(&mut pop_buf[..want]);
                    let frames = got / 2;
                    for f in 0..frames {
                        let (l, r) = (pop_buf[f * 2], pop_buf[f * 2 + 1]);
                        let dst = &mut data[f * device_channels..(f + 1) * device_channels];
                        if device_channels == 1 {
                            dst[0] = 0.5 * (l + r);
                        } else {
                            dst[0] = l;
                            dst[1] = r;
                        }
                    }
                    media_frames = frames as u64;
                    cb_clock.advance(media_frames);
                }

                // 3) Refresh the latency measurement every callback.
                let ts = info.timestamp();
                let latency = ts.playback.duration_since(ts.callback);
                cb_clock.record_callback(latency.as_nanos() as u64, media_frames);
            },
            move |err: cpal::Error| {
                // Xrun = underrun; ALSA/pipewire recover automatically.
                if !matches!(err.kind(), cpal::ErrorKind::Xrun) {
                    eprintln!("cutty-audio: stream error: {err}");
                }
            },
            None,
        )?;
        stream.play()?; // streams never auto-start in cpal 0.18

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let render_thread = {
            let clock = clock.clone();
            std::thread::Builder::new()
                .name("cutty-mixer".into())
                .spawn(move || {
                    run_render(producer, clock, cmd_rx, out_rate, on_error, source_factory)
                })?
        };

        Ok(Self {
            clock,
            cmd_tx,
            render_thread: Some(render_thread),
            _stream: stream,
            out_rate,
        })
    }

    /// The shared master clock (video presentation chases this).
    pub fn clock(&self) -> Arc<PlaybackClock> {
        self.clock.clone()
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    pub fn play(&self) {
        self.clock.set_playing(true);
    }

    pub fn pause(&self) {
        self.clock.set_playing(false);
    }

    pub fn is_playing(&self) -> bool {
        self.clock.is_playing()
    }

    pub fn position_secs(&self) -> f64 {
        self.clock.position_secs()
    }

    /// Replace the mixed timeline (applied by the render thread; audible
    /// within one ring drain).
    pub fn set_timeline(&self, timeline: MixerTimeline) {
        let _ = self.cmd_tx.send(MixerCmd::SetTimeline(timeline));
    }

    /// Seek to timeline `secs`. Returns immediately; the clock rebases
    /// when the audio callback drains the ring (a few milliseconds).
    pub fn seek(&self, secs: f64) {
        let _ = self.cmd_tx.send(MixerCmd::Seek(secs.max(0.0)));
    }
}

impl Drop for TimelineAudio {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(MixerCmd::Stop);
        if let Some(t) = self.render_thread.take() {
            let _ = t.join();
        }
    }
}

/// Render-thread main loop: keep the ring topped up with mixed blocks;
/// apply timeline swaps and seeks through the rebase handshake.
fn run_render(
    mut producer: impl Producer<Item = f32>,
    clock: Arc<PlaybackClock>,
    commands: Receiver<MixerCmd>,
    out_rate: u32,
    on_error: Box<dyn Fn(String) + Send>,
    mut source_factory: SourceFactory,
) {
    let mut segments: Vec<PlacedSegment> = Vec::new();
    let mut readers: HashMap<usize, Option<ClipReader>> = HashMap::new();
    let mut head: i64 = 0;
    let mut block = vec![0f32; BLOCK_FRAMES * 2];
    let mut open =
        |seg: &AudioSegment| -> Result<Box<dyn AudioSource>, AudioError> { source_factory(&seg.path) };
    // Report each failing path once — a broken clip must not spam.
    let mut reported: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut on_error = move |seg: &AudioSegment, msg: String| {
        if reported.insert(seg.path.clone()) {
            on_error(format!("audio for {}: {msg}", seg.path.display()));
        }
    };

    loop {
        let can_fill = !clock.rebase_pending() && producer.vacant_len() >= BLOCK_FRAMES * 2;
        let cmd = if can_fill {
            match commands.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            match commands.recv_timeout(IDLE_POLL) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };

        match cmd {
            Some(MixerCmd::Stop) => return,
            Some(MixerCmd::Seek(secs)) => {
                head = (secs * f64::from(out_rate)).round().max(0.0) as i64;
                readers.clear();
                // The callback drains stale samples and rebases; we must
                // not push until that lands (rebase_pending gates filling).
                clock.request_rebase(head as u64);
                continue;
            }
            Some(MixerCmd::SetTimeline(timeline)) => {
                segments = place(timeline, out_rate);
                readers.clear();
                // Resync at the current position: the ring holds samples
                // mixed from the old timeline; drain them.
                head = (clock.position_secs() * f64::from(out_rate)).round() as i64;
                clock.request_rebase(head as u64);
                continue;
            }
            None => {}
        }

        if can_fill {
            render_block(
                &segments,
                &mut readers,
                &mut open,
                &mut on_error,
                head,
                out_rate,
                &mut block,
            );
            producer.push_slice(&block);
            head += BLOCK_FRAMES as i64;
            evict_readers(&segments, &mut readers, head, out_rate);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake source: sample value == absolute frame index (exact in f32 up
    /// to 2^24), any rate/channel layout. Stereo variant negates R.
    struct IndexSource {
        rate: u32,
        channels: usize,
        pos: i64,
        len: i64,
    }

    impl IndexSource {
        fn mono(rate: u32, len: i64) -> Self {
            Self {
                rate,
                channels: 1,
                pos: 0,
                len,
            }
        }

        fn stereo(rate: u32, len: i64) -> Self {
            Self {
                rate,
                channels: 2,
                pos: 0,
                len,
            }
        }
    }

    impl AudioSource for IndexSource {
        fn sample_rate(&self) -> u32 {
            self.rate
        }

        fn channels(&self) -> usize {
            self.channels
        }

        fn seek(&mut self, secs: f64) -> Result<(), AudioError> {
            self.pos = (secs * f64::from(self.rate)).round() as i64;
            Ok(())
        }

        fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
            let frames = (out.len() / self.channels).min((self.len - self.pos).max(0) as usize);
            for f in 0..frames {
                let v = (self.pos + f as i64) as f32;
                for c in 0..self.channels {
                    out[f * self.channels + c] = if c == 1 { -v } else { v };
                }
            }
            self.pos += frames as i64;
            Ok(frames * self.channels)
        }
    }

    const RATE: u32 = 48_000;

    fn seg(t_in: f64, t_out: f64, source_in: f64, volume: f64) -> AudioSegment {
        AudioSegment {
            path: PathBuf::from("/fake"),
            timeline_in: t_in,
            timeline_out: t_out,
            source_in,
            speed: 1.0,
            volume,
            envelope: None,
            fade_in: None,
            fade_out: None,
        }
    }

    /// Render `frames` starting at timeline frame `head` with mono
    /// index-valued sources of `src_rate`.
    fn render(
        segments: &[AudioSegment],
        head: i64,
        frames: usize,
        src_rate: u32,
    ) -> (Vec<f32>, Vec<String>) {
        let placed = place(
            MixerTimeline {
                segments: segments.to_vec(),
            },
            RATE,
        );
        let mut readers = HashMap::new();
        let mut errors = Vec::new();
        let mut out = vec![0f32; frames * 2];
        render_block(
            &placed,
            &mut readers,
            &mut |_| Ok(Box::new(IndexSource::mono(src_rate, i64::MAX / 2))),
            &mut |_, e| errors.push(e),
            head,
            RATE,
            &mut out,
        );
        (out, errors)
    }

    #[test]
    fn same_rate_segment_maps_source_frames_exactly() {
        // Timeline [1s, 2s) ← source from 0.5s. At timeline frame
        // 48000+k the source frame is 24000+k. Volume keeps the raw
        // index values inside the mixer's [-1, 1] output clamp.
        let v = 1e-6;
        let segs = [seg(1.0, 2.0, 0.5, v)];
        let (out, errors) = render(&segs, 48_000, 64, RATE);
        assert!(errors.is_empty());
        for k in 0..64usize {
            let expected = v as f32 * (24_000 + k) as f32;
            assert!(
                (out[k * 2] - expected).abs() < 1e-9,
                "L at {k}: {} vs {expected}",
                out[k * 2]
            );
            assert_eq!(out[k * 2], out[k * 2 + 1], "R duplicates mono");
        }
    }

    #[test]
    fn gaps_and_out_of_range_render_silence() {
        let segs = [seg(1.0, 2.0, 0.0, 1.0)];
        // Entirely before the segment.
        let (out, _) = render(&segs, 0, 32, RATE);
        assert!(out.iter().all(|&s| s == 0.0), "silence before");
        // Entirely after.
        let (out, _) = render(&segs, 3 * 48_000, 32, RATE);
        assert!(out.iter().all(|&s| s == 0.0), "silence after");
    }

    #[test]
    fn volume_scales_and_overlaps_sum() {
        // Two overlapping segments: [0,1) vol 0.25 src 0, [0,1) vol 0.5
        // src 1s → out = 0.25*k + 0.5*(48000+k). Values exceed [-1,1] —
        // use a tiny scale via volume to stay under the clamp.
        let v1 = 1e-6;
        let v2 = 2e-6;
        let segs = [seg(0.0, 1.0, 0.0, v1), seg(0.0, 1.0, 1.0, v2)];
        let (out, _) = render(&segs, 100, 16, RATE);
        for k in 0..16 {
            let t = (100 + k) as f64;
            let expected = (v1 * t + v2 * (48_000.0 + t)) as f32;
            assert!(
                (out[k as usize * 2] - expected).abs() < 1e-6,
                "frame {k}: {} vs {expected}",
                out[k as usize * 2]
            );
        }
    }

    #[test]
    fn cut_between_touching_segments_is_sample_exact() {
        // A: [0, 0.5) from source 0; B: [0.5, 1.0) from source 2s.
        // Use volume to keep values in clamp range.
        let v = 1e-6;
        let segs = [seg(0.0, 0.5, 0.0, v), seg(0.5, 1.0, 2.0, v)];
        let cut = 24_000i64;
        // Block straddles the cut.
        let (out, _) = render(&segs, cut - 4, 8, RATE);
        for k in 0..8i64 {
            let t = cut - 4 + k;
            let expected = if t < cut {
                v as f32 * t as f32 // A: source frame == timeline frame
            } else {
                v as f32 * (2.0 * 48_000.0 + (t - cut) as f32) // B from 2 s
            };
            let got = out[k as usize * 2];
            assert!(
                (got - expected).abs() < 1e-9,
                "frame {t}: got {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn upsampling_interpolates_linearly() {
        // 24 kHz source into a 48 kHz timeline: output frame k sits at
        // source position k/2 — linear interpolation of an index ramp is
        // exact: value = k/2.
        let v = 1e-6;
        let segs = [seg(0.0, 1.0, 0.0, v)];
        let (out, _) = render(&segs, 0, 64, 24_000);
        for k in 0..64usize {
            let expected = v as f32 * (k as f32 / 2.0);
            assert!(
                (out[k * 2] - expected).abs() < 1e-9,
                "frame {k}: {} vs {expected}",
                out[k * 2]
            );
        }
    }

    #[test]
    fn source_eof_pads_with_silence() {
        // Source only 100 frames long; segment asks for a full second.
        let segs = [seg(0.0, 1.0, 0.0, 1e-6)];
        let placed = place(
            MixerTimeline {
                segments: segs.to_vec(),
            },
            RATE,
        );
        let mut readers = HashMap::new();
        let mut out = vec![0f32; 256 * 2];
        render_block(
            &placed,
            &mut readers,
            &mut |_| Ok(Box::new(IndexSource::mono(RATE, 100))),
            &mut |_, _| {},
            0,
            RATE,
            &mut out,
        );
        assert!(out[99 * 2] != 0.0, "last real frame present");
        assert_eq!(out[101 * 2], 0.0, "past EOF is silence");
        assert_eq!(out[255 * 2], 0.0);
    }

    #[test]
    fn failed_open_reports_once_and_renders_silence() {
        let segs = [seg(0.0, 1.0, 0.0, 1.0)];
        let placed = place(
            MixerTimeline {
                segments: segs.to_vec(),
            },
            RATE,
        );
        let mut readers = HashMap::new();
        let mut errors = Vec::new();
        let mut out = vec![0f32; 32 * 2];
        for _ in 0..3 {
            render_block(
                &placed,
                &mut readers,
                &mut |_| Err(AudioError::NoAudioTrack),
                &mut |_, e| errors.push(e),
                0,
                RATE,
                &mut out,
            );
        }
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(errors.len(), 1, "one report per segment, not per block");
    }

    #[test]
    fn stereo_source_keeps_channels() {
        let segs = [seg(0.0, 1.0, 0.0, 1e-6)];
        let placed = place(
            MixerTimeline {
                segments: segs.to_vec(),
            },
            RATE,
        );
        let mut readers = HashMap::new();
        let mut out = vec![0f32; 16 * 2];
        render_block(
            &placed,
            &mut readers,
            &mut |_| Ok(Box::new(IndexSource::stereo(RATE, i64::MAX / 2))),
            &mut |_, _| {},
            1000,
            RATE,
            &mut out,
        );
        for k in 0..16usize {
            let v = 1e-6f32 * (1000 + k as i64) as f32;
            assert!((out[k * 2] - v).abs() < 1e-9, "L");
            assert!((out[k * 2 + 1] + v).abs() < 1e-9, "R negated by fixture");
        }
    }

    #[test]
    fn nonsequential_blocks_reposition_the_reader() {
        // Render at 0, then jump to 0.75s within the same reader map —
        // the reader must reposition, not continue sequentially.
        let v = 1e-6;
        let segs = [seg(0.0, 1.0, 0.0, v)];
        let placed = place(
            MixerTimeline {
                segments: segs.to_vec(),
            },
            RATE,
        );
        let mut readers = HashMap::new();
        let mut open = |_: &AudioSegment| -> Result<Box<dyn AudioSource>, AudioError> {
            Ok(Box::new(IndexSource::mono(RATE, i64::MAX / 2)))
        };
        let mut out = vec![0f32; 8 * 2];
        render_block(
            &placed,
            &mut readers,
            &mut open,
            &mut |_, _| {},
            0,
            RATE,
            &mut out,
        );
        assert_eq!(out[0], 0.0);
        assert!((out[2] - v as f32).abs() < 1e-9);

        let jump = 36_000i64;
        render_block(
            &placed,
            &mut readers,
            &mut open,
            &mut |_, _| {},
            jump,
            RATE,
            &mut out,
        );
        for k in 0..8usize {
            let expected = v as f32 * (jump + k as i64) as f32;
            assert!(
                (out[k * 2] - expected).abs() < 1e-8,
                "frame {k}: {} vs {expected}",
                out[k * 2]
            );
        }
    }

    #[test]
    fn transition_crossfade_applies_equal_power_ramps_per_sample() {
        // Outgoing constant-source segment fading out over [0.5, 1.0],
        // incoming fading in over the same span: at every sample the two
        // gains must be cos/sin of the ramp position, and the summed
        // power of a unity crossfade stays 1.
        let v = 0.5;
        let ramp = FadeRamp {
            start: 0.5,
            end: 1.0,
        };
        let mut out_seg = seg(0.0, 1.0, 0.0, v);
        out_seg.fade_out = Some(ramp);
        let mut in_seg = seg(0.5, 1.5, 0.0, v);
        in_seg.fade_in = Some(ramp);

        let placed = place(
            MixerTimeline {
                segments: vec![out_seg, in_seg],
            },
            RATE,
        );
        let mut readers = HashMap::new();
        let mut out = vec![0f32; 64 * 2];
        let head = (0.75 * f64::from(RATE)) as i64; // mid-ramp
        render_block(
            &placed,
            &mut readers,
            &mut |_| Ok(Box::new(ConstOne)),
            &mut |_, e| panic!("{e}"),
            head,
            RATE,
            &mut out,
        );
        for k in 0..64usize {
            let t = (head + k as i64) as f64 / f64::from(RATE);
            let x = (t - ramp.start) / (ramp.end - ramp.start);
            let expected = v as f32
                * ((x * std::f64::consts::FRAC_PI_2).cos() as f32
                    + (x * std::f64::consts::FRAC_PI_2).sin() as f32);
            assert!(
                (out[k * 2] - expected).abs() < 1e-5,
                "sample {k}: {} vs {expected}",
                out[k * 2]
            );
        }
        // At the exact crossover both sides sit at √½ · v.
        let mut out = vec![0f32; 2];
        render_block(
            &placed,
            &mut readers,
            &mut |_| Ok(Box::new(ConstOne)),
            &mut |_, e| panic!("{e}"),
            (0.75 * f64::from(RATE)) as i64,
            RATE,
            &mut out,
        );
        let expected = v as f32 * 2.0 * std::f64::consts::FRAC_1_SQRT_2 as f32;
        assert!((out[0] - expected).abs() < 1e-5, "{} vs {expected}", out[0]);
    }

    /// Constant 1.0 mono source at the output rate (crossfade math test).
    struct ConstOne;

    impl AudioSource for ConstOne {
        fn sample_rate(&self) -> u32 {
            RATE
        }

        fn channels(&self) -> usize {
            1
        }

        fn seek(&mut self, _secs: f64) -> Result<(), AudioError> {
            Ok(())
        }

        fn read(&mut self, out: &mut [f32]) -> Result<usize, AudioError> {
            out.fill(1.0);
            Ok(out.len())
        }
    }

    #[test]
    fn clamps_summed_overloads() {
        let segs = [seg(0.0, 1.0, 0.0, 10.0)];
        let (out, _) = render(&segs, 48_000 / 2, 8, RATE);
        assert!(out.iter().all(|&s| (-1.0..=1.0).contains(&s)));
        assert_eq!(out[2], 1.0, "saturated at full scale");
    }

    #[test]
    fn soft_clip_is_transparent_below_the_knee_and_saturates_above() {
        for s in [-0.875f32, -0.5, -0.001, 0.0, 0.3, 0.7, SOFT_CLIP_KNEE] {
            assert_eq!(soft_clip(s), s, "identity below the knee");
        }
        // C¹ at the knee: just above it stays just above it.
        let eps = 1e-4f32;
        let just_over = soft_clip(SOFT_CLIP_KNEE + eps);
        assert!((just_over - (SOFT_CLIP_KNEE + eps)).abs() < 1e-5);
        // Monotonic, bounded, symmetric.
        let mut prev = 0.0f32;
        for i in 0..200 {
            let x = i as f32 * 0.05;
            let y = soft_clip(x);
            assert!(y >= prev, "monotonic at {x}");
            assert!(y <= 1.0, "bounded at {x}");
            assert_eq!(soft_clip(-x), -y, "odd-symmetric at {x}");
            prev = y;
        }
        assert_eq!(soft_clip(100.0), 1.0, "full saturation");
    }

    /// One point of the mixer's volume-envelope evaluation semantics.
    fn pt(t: f64, value: f64, easing: Easing) -> EnvelopePoint {
        EnvelopePoint { t, value, easing }
    }

    #[test]
    fn envelope_gain_holds_edges_and_eases_between() {
        let env = VolumeEnvelope {
            points: vec![
                pt(1.0, 0.0, Easing::EaseInOut),
                pt(2.0, 1.0, Easing::Linear),
            ],
        };
        assert_eq!(env.gain_at(0.0), 0.0, "first value before the range");
        assert_eq!(env.gain_at(9.0), 1.0, "last value after the range");
        assert_eq!(env.gain_at(1.5), 0.5, "smoothstep midpoint");
        let x: f64 = 0.25;
        assert!((env.gain_at(1.25) - x * x * (3.0 - 2.0 * x)).abs() < 1e-12);
    }

    #[test]
    fn envelope_applies_per_sample_with_no_block_steps() {
        // Constant-1.0 source; envelope ramps 0 → 1 over [0.25, 0.75].
        // Every output sample must sit exactly on the ramp — a per-block
        // gain would show staircase plateaus.
        let mut s = seg(0.0, 1.0, 0.0, 0.5);
        s.envelope = Some(VolumeEnvelope {
            points: vec![pt(0.25, 0.0, Easing::Linear), pt(0.75, 1.0, Easing::Linear)],
        });
        let placed = place(MixerTimeline { segments: vec![s] }, RATE);
        let mut readers = HashMap::new();
        let mut out = vec![0f32; 512 * 2];
        let head = (0.25 * f64::from(RATE)) as i64 - 64;
        render_block(
            &placed,
            &mut readers,
            &mut |_| Ok(Box::new(ConstOne)),
            &mut |_, e| panic!("{e}"),
            head,
            RATE,
            &mut out,
        );
        for k in 0..512usize {
            let t = (head + k as i64) as f64 / f64::from(RATE);
            let ramp = ((t - 0.25) / 0.5).clamp(0.0, 1.0);
            let expected = 0.5 * ramp as f32; // volume × envelope
            assert!(
                (out[k * 2] - expected).abs() < 1e-6,
                "sample {k}: {} vs {expected}",
                out[k * 2]
            );
        }
    }

    #[test]
    fn envelope_composes_with_volume_and_transition_ramps() {
        // Envelope at constant 0.5 under a fade-out transition ramp:
        // gain = volume × envelope × cos-ramp, per sample.
        let ramp = FadeRamp {
            start: 0.0,
            end: 1.0,
        };
        let mut s = seg(0.0, 1.0, 0.0, 0.8);
        s.envelope = Some(VolumeEnvelope {
            points: vec![pt(0.0, 0.5, Easing::Linear)],
        });
        s.fade_out = Some(ramp);
        let placed = place(MixerTimeline { segments: vec![s] }, RATE);
        let mut readers = HashMap::new();
        let mut out = vec![0f32; 64 * 2];
        let head = (0.5 * f64::from(RATE)) as i64;
        render_block(
            &placed,
            &mut readers,
            &mut |_| Ok(Box::new(ConstOne)),
            &mut |_, e| panic!("{e}"),
            head,
            RATE,
            &mut out,
        );
        for k in 0..64usize {
            let t = (head + k as i64) as f64 / f64::from(RATE);
            let expected = 0.8 * 0.5 * ramp.gain_out(t);
            assert!(
                (out[k * 2] - expected).abs() < 1e-6,
                "sample {k}: {} vs {expected}",
                out[k * 2]
            );
        }
    }

    #[test]
    fn four_tracks_sum_transparently_below_the_knee_and_compress_above() {
        // 4 × 0.2 = 0.8 < knee: bit-exact linear sum.
        let quiet: Vec<AudioSegment> = (0..4).map(|_| seg(0.0, 1.0, 0.0, 0.2)).collect();
        let placed = place(
            MixerTimeline {
                segments: quiet.clone(),
            },
            RATE,
        );
        let mut readers = HashMap::new();
        let mut out = vec![0f32; 16 * 2];
        render_block(
            &placed,
            &mut readers,
            &mut |_| Ok(Box::new(ConstOne)),
            &mut |_, e| panic!("{e}"),
            100,
            RATE,
            &mut out,
        );
        for s in out.iter() {
            assert!((s - 0.8).abs() < 1e-6, "transparent sum: {s}");
        }

        // 4 × 0.3 = 1.2 > knee: compressed smoothly under full scale,
        // never wrapped or hard-flattened to exactly 1.0.
        let loud: Vec<AudioSegment> = (0..4).map(|_| seg(0.0, 1.0, 0.0, 0.3)).collect();
        let placed = place(MixerTimeline { segments: loud }, RATE);
        let mut readers = HashMap::new();
        render_block(
            &placed,
            &mut readers,
            &mut |_| Ok(Box::new(ConstOne)),
            &mut |_, e| panic!("{e}"),
            100,
            RATE,
            &mut out,
        );
        let expected = soft_clip(1.2);
        assert!(expected > SOFT_CLIP_KNEE && expected < 1.0);
        for s in out.iter() {
            assert!((s - expected).abs() < 1e-6, "soft-clamped sum: {s}");
        }
    }

    #[test]
    fn place_drops_degenerate_segments() {
        let placed = place(
            MixerTimeline {
                segments: vec![
                    seg(1.0, 1.0, 0.0, 1.0),      // zero-length
                    seg(f64::NAN, 2.0, 0.0, 1.0), // non-finite
                    seg(3.0, 2.0, 0.0, 1.0),      // inverted
                    seg(0.0, 1.0, 0.0, 1.0),      // valid
                ],
            },
            RATE,
        );
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].start_frame, 0);
        assert_eq!(placed[0].end_frame, 48_000);
    }
}
