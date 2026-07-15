//! Timeline playback: plays the project across clips, cuts, and gaps.
//!
//! Phase 2 frame pipeline: the **output frame grid drives everything**.
//! Each output frame (at project fps) resolves the full video layer stack
//! ([`cutty_engine::resolve_video_layers`], bottom→top), samples the
//! latest decoded frame ≤ the clip's source time per layer, and runs the
//! GPU compositor ([`crate::compose::TimelineRenderer`]) at preview
//! resolution — the same renderer the export frontend uses, so what you
//! preview is what exports. The transport from there is unchanged:
//! readback → JPEG → binary IPC → canvas.
//!
//! - **Audio is the master clock.** [`cutty_audio::TimelineAudio`] mixes
//!   every audio-contributing clip sample-accurately (gaps = silence, so
//!   the clock never stalls while playing). Video presentation *chases*
//!   that clock — late frames are dropped, the clock is never adjusted.
//! - **One decoder session per active source across all tracks.**
//!   ~[`LOOKAHEAD`] before a cut, a decoder for the incoming source is
//!   opened and positioned on a prefetch thread, so crossing the cut
//!   never pays an open+seek on the render path. Sessions for sources
//!   out of range are closed. Split points (same source, contiguous
//!   ranges) reuse the running session and need no priming at all.
//! - **Gaps** render a black frame and silence; playback continues.
//! - **Scrubbing** while paused coalesces seek requests (only the latest
//!   matters) and serves visited frames from the output-frame-keyed
//!   [`FrameCache`]; cold positions pay one in-process seek + composite
//!   (<100 ms).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cutty_audio::{MixerTimeline, TimelineAudio};
use cutty_engine::{
    resolve_track_visuals, resolve_video_layers, timeline_end, transition_spans, Project,
    ProjectSettings, TrackKind, TrackVisual,
};

use crate::compose::TimelineRenderer;
use crate::decode::SourceDecoder;
use crate::error::MediaError;
use crate::framecache::{CachedFrame, FrameCache, DEFAULT_CAPACITY_BYTES};
use crate::jpeg::JpegEncoder;
use crate::proxy::proxy_path_for;

/// Seconds of lookahead before a cut at which the incoming source's
/// decoder is opened and positioned.
const LOOKAHEAD: f64 = 0.5;
/// Grid frames later than this behind the clock are skipped instead of
/// shown.
const DROP_THRESHOLD_FRAMES: f64 = 1.5;
/// Poll granularity while waiting for the clock to reach a frame's pts.
const CLOCK_POLL: Duration = Duration::from_millis(2);
/// Cadence of position events while playing.
const POSITION_EVENT_INTERVAL: Duration = Duration::from_millis(250);
/// How long the master clock may sit still during playback before the
/// engine assumes the device died and freewheels (the mixer renders
/// silence through gaps, so a healthy clock never stalls).
const CLOCK_STALL_THRESHOLD: Duration = Duration::from_millis(400);
/// Matching tolerance for "contiguous source ranges" at a split point,
/// seconds (well under one frame at any real rate).
const CONTINUITY_EPS: f64 = 1e-3;

/// Events pushed from the playback engine to the embedder.
pub enum PlayerEvent {
    /// A frame due for presentation *now*. `pts_sec` is timeline time.
    Frame {
        pts_sec: f64,
        /// Master-clock reading at presentation. `pts_sec - clock_sec` is
        /// the instantaneous A/V offset (≈0 when in sync).
        clock_sec: f64,
        width: u32,
        height: u32,
        jpeg: Vec<u8>,
    },
    /// Transport/position report (timeline seconds).
    Position { position_sec: f64, playing: bool },
    /// Playback reached the end of the timeline (transport is paused).
    Eof,
    /// A non-fatal engine error worth surfacing.
    Error(String),
}

pub type EventSink = Box<dyn Fn(PlayerEvent) + Send + 'static>;

enum Cmd {
    SetProject(Box<Project>),
    Play,
    Pause,
    TogglePlay,
    Seek(f64),
    Step(i64),
    /// Proxy generation finished (or media changed on disk): re-resolve
    /// source files and refresh what's on screen.
    RefreshSources,
    Stop,
}

/// Handle to the running playback engine. Dropping it stops playback and
/// tears down all decode sessions.
pub struct TimelinePlayer {
    cmd_tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl TimelinePlayer {
    /// Start the engine (paused, at 0) on a project snapshot.
    pub fn open(project: Project, sink: EventSink) -> Result<Self, MediaError> {
        let jpeg = JpegEncoder::new()?; // fail fast if turbojpeg is broken
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("cutty-playback".into())
            .spawn(move || run(project, jpeg, sink, cmd_rx))?;
        Ok(Self {
            cmd_tx,
            thread: Some(thread),
        })
    }

    /// Swap in a new project snapshot (after any engine mutation).
    pub fn set_project(&self, project: Project) {
        let _ = self.cmd_tx.send(Cmd::SetProject(Box::new(project)));
    }

    pub fn play(&self) {
        let _ = self.cmd_tx.send(Cmd::Play);
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(Cmd::Pause);
    }

    pub fn toggle_play(&self) {
        let _ = self.cmd_tx.send(Cmd::TogglePlay);
    }

    /// Seek/scrub to timeline seconds (paused: shows the frame there).
    pub fn seek(&self, secs: f64) {
        let _ = self.cmd_tx.send(Cmd::Seek(secs));
    }

    /// Step `delta` project frames (negative = backwards). Pauses.
    pub fn step(&self, delta: i64) {
        let _ = self.cmd_tx.send(Cmd::Step(delta));
    }

    /// Re-resolve media files (a proxy finished generating).
    pub fn refresh_sources(&self) {
        let _ = self.cmd_tx.send(Cmd::RefreshSources);
    }
}

impl Drop for TimelinePlayer {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------
// Preview output resolution
// ---------------------------------------------------------------------

/// The compositor's preview canvas: the project aspect fit within
/// 1280×720, never upscaled past project size, even dimensions (proxies
/// are ≤720p, so preview pixels come from ≤1:1 proxy samples).
fn preview_size(settings: &ProjectSettings) -> (u32, u32) {
    let pw = f64::from(settings.width.max(2));
    let ph = f64::from(settings.height.max(2));
    let scale = (1280.0 / pw).min(720.0 / ph).min(1.0);
    let even = |v: f64| (((v / 2.0).round() as u32) * 2).max(2);
    (even(pw * scale), even(ph * scale))
}

// ---------------------------------------------------------------------
// Timeline queries beyond the resolver
// ---------------------------------------------------------------------

/// A video clip that starts (visually) within the lookahead window.
struct UpcomingClip {
    clip_id: u64,
    media_id: u64,
    /// When the clip first appears on screen: its `timeline_in`, or the
    /// transition span start when it enters through a transition.
    timeline_in: f64,
    /// The source position needed at that first appearance (negative
    /// values — an incoming freeze side — clamp to 0 at the decoder).
    source_in: f64,
    /// Entering mid-transition: both sides stream, so the running
    /// decoder can never be reused for this clip.
    via_transition: bool,
}

/// Video clips on visible tracks whose first on-screen moment falls in
/// `(t, t + horizon]`. Transition targets appear at their span start —
/// half a transition *before* their cut — with the extended source
/// position, so priming completes before the overlap begins.
fn upcoming_video_starts(project: &Project, t: f64, horizon: f64) -> Vec<UpcomingClip> {
    let spans = transition_spans(project);
    let mut upcoming: Vec<UpcomingClip> = project
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Video && !track.hidden)
        .flat_map(|track| {
            let spans = &spans;
            track.clips.iter().filter_map(move |clip| {
                let span = spans.iter().find(|s| s.to_clip == clip.id);
                let (start, source_in, via_transition) = match span {
                    Some(s) => (
                        s.start,
                        clip.source_in - (s.cut - s.start) * clip.speed,
                        true,
                    ),
                    None => (clip.timeline_in, clip.source_in, false),
                };
                (t < start && start <= t + horizon).then_some(UpcomingClip {
                    clip_id: clip.id.0,
                    media_id: clip.media_id.0,
                    timeline_in: start,
                    source_in,
                    via_transition,
                })
            })
        })
        .collect();
    upcoming.sort_by(|a, b| a.timeline_in.total_cmp(&b.timeline_in));
    upcoming
}

/// Whether `upcoming` is a pure split-point continuation of a clip active
/// at `t` (same source, contiguous timeline and source ranges) — the
/// running decoder flows straight into it, no priming needed.
fn is_continuation(project: &Project, upcoming: &UpcomingClip, t: f64) -> bool {
    resolve_video_layers(project, t).iter().any(|active| {
        project.find_clip(active.clip_id).is_some_and(|(_, cur)| {
            cur.media_id.0 == upcoming.media_id
                && (cur.timeline_out - upcoming.timeline_in).abs() < CONTINUITY_EPS
                && (cur.source_out - upcoming.source_in).abs() < CONTINUITY_EPS
        })
    })
}

/// Everything the mixer needs: one [`AudioSegment`] per audio-contributing
/// clip on an unmuted track. Video-track clips contribute their media's
/// audio (scaled by clip volume) from the proxy; audio-track clips play
/// from their resolved file. Muting a track silences it either way.
/// Video transitions extend the two clips across the span with
/// equal-power ramps ([`crate::audio_layout`]); music tracks are
/// untouched.
fn mixer_timeline(project: &Project, sources: &Sources) -> MixerTimeline {
    let spans = transition_spans(project);
    let mut segments = Vec::new();
    for track in project.tracks.iter().filter(|t| !t.muted) {
        for clip in &track.clips {
            let Some(media) = project.media(clip.media_id) else {
                continue;
            };
            if !media.has_audio {
                continue;
            }
            let Some(path) = sources.audio_path(clip.media_id.0) else {
                continue; // proxy not ready yet — silent until refresh
            };
            let placement = crate::audio_layout::audio_placement(clip, &spans);
            segments.push(cutty_audio::AudioSegment {
                path,
                timeline_in: placement.timeline_in,
                timeline_out: placement.timeline_out,
                source_in: placement.source_in,
                speed: clip.speed,
                volume: clip.volume,
                fade_in: placement.fade_in,
                fade_out: placement.fade_out,
            });
        }
    }
    MixerTimeline { segments }
}

// ---------------------------------------------------------------------
// Source resolution: media id → decodable files
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
enum SourceFiles {
    /// Video media with its proxy present (audio, when the media has it,
    /// also comes from the proxy — normalized stereo AAC).
    Ready { proxy: PathBuf, has_audio: bool },
    /// Video media whose proxy hasn't been generated yet: renders black/
    /// silent until [`TimelinePlayer::refresh_sources`].
    ProxyPending,
    /// Audio-only media: decoded straight from the original.
    AudioOnly { original: PathBuf },
}

#[derive(Default)]
struct Sources {
    map: HashMap<u64, SourceFiles>,
}

impl Sources {
    fn rebuild(&mut self, project: &Project) {
        self.map.clear();
        for media in &project.media {
            let path = PathBuf::from(&media.path);
            let entry = if media.has_video {
                match proxy_path_for(&path) {
                    Ok((proxy, true)) => SourceFiles::Ready {
                        proxy,
                        has_audio: media.has_audio,
                    },
                    _ => SourceFiles::ProxyPending,
                }
            } else if media.has_audio {
                SourceFiles::AudioOnly { original: path }
            } else {
                continue;
            };
            self.map.insert(media.id.0, entry);
        }
    }

    fn video_path(&self, media_id: u64) -> Option<PathBuf> {
        match self.map.get(&media_id)? {
            SourceFiles::Ready { proxy, .. } => Some(proxy.clone()),
            _ => None,
        }
    }

    fn audio_path(&self, media_id: u64) -> Option<PathBuf> {
        match self.map.get(&media_id)? {
            SourceFiles::Ready {
                proxy,
                has_audio: true,
            } => Some(proxy.clone()),
            SourceFiles::AudioOnly { original } => Some(original.clone()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------
// Clock: the audio mixer, or a wall clock when no device exists
// ---------------------------------------------------------------------

enum Clock {
    Audio(TimelineAudio),
    /// No usable audio device: wall-clock pacing so video still plays.
    Freewheel {
        base: f64,
        started: Option<Instant>,
    },
}

impl Clock {
    fn play(&mut self) {
        match self {
            Clock::Audio(a) => a.play(),
            Clock::Freewheel { started, .. } => {
                if started.is_none() {
                    *started = Some(Instant::now());
                }
            }
        }
    }

    fn pause(&mut self) {
        match self {
            Clock::Audio(a) => a.pause(),
            Clock::Freewheel { base, started } => {
                if let Some(t) = started.take() {
                    *base += t.elapsed().as_secs_f64();
                }
            }
        }
    }

    fn seek(&mut self, secs: f64) {
        match self {
            Clock::Audio(a) => a.seek(secs),
            Clock::Freewheel { base, started } => {
                *base = secs;
                if started.is_some() {
                    *started = Some(Instant::now());
                }
            }
        }
    }

    fn position(&self) -> f64 {
        match self {
            Clock::Audio(a) => a.position_secs(),
            Clock::Freewheel { base, started } => {
                *base + started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0)
            }
        }
    }

    fn is_playing(&self) -> bool {
        match self {
            Clock::Audio(a) => a.is_playing(),
            Clock::Freewheel { started, .. } => started.is_some(),
        }
    }

    fn set_timeline(&self, timeline: MixerTimeline) {
        if let Clock::Audio(a) = self {
            a.set_timeline(timeline);
        }
    }
}

// ---------------------------------------------------------------------
// Prefetch: decoder priming off the control thread
// ---------------------------------------------------------------------

/// Request to open + position a decoder for an upcoming clip.
struct PrimeRequest {
    clip_id: u64,
    media_id: u64,
    path: PathBuf,
    source_in: f64,
}

/// A matured prime, ready to install.
struct PrimedDecoder {
    clip_id: u64,
    media_id: u64,
    source_in: f64,
    decoder: SourceDecoder,
}

/// A prime result: a positioned decoder, or `(clip id, error)`.
type PrimeResult = Result<PrimedDecoder, (u64, String)>;

struct Prefetcher {
    req_tx: Sender<PrimeRequest>,
    res_rx: Receiver<PrimeResult>,
    _thread: JoinHandle<()>,
}

impl Prefetcher {
    fn start() -> Result<Self, MediaError> {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<PrimeRequest>();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("cutty-prefetch".into())
            .spawn(move || {
                while let Ok(req) = req_rx.recv() {
                    let result = prime(&req).map_err(|e| (req.clip_id, e.to_string()));
                    if res_tx.send(result).is_err() {
                        return;
                    }
                }
            })?;
        Ok(Self {
            req_tx,
            res_rx,
            _thread: thread,
        })
    }

    fn request(&mut self, req: PrimeRequest) {
        let _ = self.req_tx.send(req);
    }
}

/// Open a decoder and position it on the clip's first visible frame. The
/// decoded frame stays in the decoder's buffer — installing it uploads
/// that frame without touching the stream again.
fn prime(req: &PrimeRequest) -> Result<PrimedDecoder, MediaError> {
    let mut decoder = SourceDecoder::open(&req.path)?;
    decoder.seek_to(req.source_in)?;
    Ok(PrimedDecoder {
        clip_id: req.clip_id,
        media_id: req.media_id,
        source_in: req.source_in,
        decoder,
    })
}

// ---------------------------------------------------------------------
// The control loop
// ---------------------------------------------------------------------

/// An encoded frame held between pump iterations (e.g. when a command
/// interrupted the pacing wait) so it is presented, not lost.
struct EncodedFrame {
    timeline_pts: f64,
    width: u32,
    height: u32,
    jpeg: Vec<u8>,
}

/// A frame submitted to the GPU whose readback hasn't been consumed.
struct InflightFrame {
    idx: i64,
    slot: usize,
}

/// Wall-clock takeover when the audio clock stops advancing (device
/// death): video keeps pacing from here. Healthy mixers never stall —
/// gaps render silence.
struct Takeover {
    engaged_at: Instant,
    anchor_pos: f64,
    clock_pos_at_engage: f64,
}

#[derive(Default)]
struct Stats {
    frames_presented: u64,
    frames_dropped: u64,
}

struct Engine {
    project: Project,
    sources: Sources,
    clock: Clock,
    /// Non-fatal error reports from the mixer's render thread.
    mixer_errors: Receiver<String>,
    sink: EventSink,
    jpeg: JpegEncoder,
    cache: FrameCache,
    /// The GPU frame renderer (`None` only when GPU init failed — the
    /// preview then shows black and reports why, audio still plays).
    renderer: Option<TimelineRenderer>,
    prefetch: Prefetcher,
    /// Prime requests in flight (clip ids); stale results are dropped.
    in_flight: HashSet<u64>,
    /// Active layer clip ids at the last rendered frame (decoder GC runs
    /// when this changes).
    prev_layer_clips: Vec<u64>,
    /// A composited frame submitted to the GPU but not yet read back —
    /// the playing pump pre-submits grid frame N+1 before encoding N, so
    /// the GPU renders while the CPU JPEG-encodes (mirroring the export
    /// frontend's double buffering). Every transport/project change
    /// flushes it.
    inflight: Option<InflightFrame>,
    /// The next output grid frame to render while playing.
    next_grid_idx: Option<i64>,
    /// A rendered-but-not-yet-presented frame (pacing wait interrupted).
    pending_frame: Option<EncodedFrame>,
    /// Last presented/known timeline position (UI position while paused,
    /// anchor for stepping).
    position: f64,
    /// The last thing shown is the gap black frame.
    showing_black: bool,
    at_eof: bool,
    takeover: Option<Takeover>,
    last_clock: (f64, Instant),
    last_position_event: Instant,
    /// Messages already surfaced (report each condition once).
    reported: HashSet<String>,
    stats: Stats,
    log: bool,
}

fn run(project: Project, jpeg: JpegEncoder, sink: EventSink, cmd_rx: Receiver<Cmd>) {
    // Mixer errors cross threads via a channel; the control loop forwards
    // them through the single sink.
    let (mix_err_tx, mix_err_rx) = std::sync::mpsc::channel::<String>();
    let clock = match TimelineAudio::open(Box::new(move |msg| {
        let _ = mix_err_tx.send(msg);
    })) {
        Ok(audio) => Clock::Audio(audio),
        Err(e) => {
            (sink)(PlayerEvent::Error(format!(
                "audio unavailable ({e}) — playing without sound"
            )));
            Clock::Freewheel {
                base: 0.0,
                started: None,
            }
        }
    };

    let prefetch = match Prefetcher::start() {
        Ok(p) => p,
        Err(e) => {
            (sink)(PlayerEvent::Error(format!("playback init failed: {e}")));
            return;
        }
    };

    let mut engine = Engine {
        sources: Sources::default(),
        clock,
        mixer_errors: mix_err_rx,
        sink,
        jpeg,
        cache: FrameCache::new(DEFAULT_CAPACITY_BYTES),
        renderer: None,
        prefetch,
        in_flight: HashSet::new(),
        prev_layer_clips: Vec::new(),
        inflight: None,
        next_grid_idx: None,
        pending_frame: None,
        position: 0.0,
        showing_black: false,
        at_eof: false,
        takeover: None,
        last_clock: (0.0, Instant::now()),
        last_position_event: Instant::now(),
        reported: HashSet::new(),
        stats: Stats::default(),
        log: std::env::var_os("CUTTY_PLAYBACK_LOG").is_some(),
        project,
    };
    engine.sources.rebuild(&engine.project);
    engine
        .clock
        .set_timeline(mixer_timeline(&engine.project, &engine.sources));
    engine.ensure_renderer();

    // Show the frame at 0 immediately so the player never opens blank
    // when there's content.
    engine.scrub_to(0.0);
    engine.emit_position();

    let mut backlog: VecDeque<Cmd> = VecDeque::new();
    loop {
        // Pull everything immediately available, so bursts of scrub seeks
        // (and drag-transient project snapshots) collapse to their latest
        // state instead of being replayed one by one.
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => backlog.push_back(cmd),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if backlog.is_empty() && !engine.clock.is_playing() {
            match cmd_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(cmd) => backlog.push_back(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        collapse_backlog(&mut backlog);

        match backlog.pop_front() {
            Some(Cmd::Stop) => return,
            Some(Cmd::SetProject(p)) => engine.apply_project(*p),
            Some(Cmd::Play) => engine.do_play(),
            Some(Cmd::Pause) => engine.do_pause(),
            Some(Cmd::TogglePlay) => {
                if engine.clock.is_playing() {
                    engine.do_pause();
                } else {
                    engine.do_play();
                }
            }
            Some(Cmd::Seek(t)) => engine.do_seek(t),
            Some(Cmd::Step(n)) => engine.do_step(n),
            Some(Cmd::RefreshSources) => engine.do_refresh(),
            None => {}
        }

        engine.poll_prefetch();
        engine.forward_mixer_errors();

        if engine.clock.is_playing() {
            if let Some(cmd) = engine.pump(&cmd_rx) {
                backlog.push_front(cmd);
            }
        }
    }
}

/// Collapse redundant queued commands: only the last of a *run* of
/// `Seek`s matters, only the last of a run of `SetProject`s (each
/// carries a full snapshot), and a run of `Step`s is one summed step
/// (key auto-repeat must not queue up seeks). Order relative to other
/// commands is preserved.
fn collapse_backlog(backlog: &mut VecDeque<Cmd>) {
    if backlog.len() < 2 {
        return;
    }
    let cmds = std::mem::take(backlog);
    for cmd in cmds {
        match (&cmd, backlog.back_mut()) {
            (Cmd::Seek(_), Some(last @ Cmd::Seek(_)))
            | (Cmd::SetProject(_), Some(last @ Cmd::SetProject(_))) => *last = cmd,
            (Cmd::Step(n), Some(Cmd::Step(m))) => *m += n,
            _ => backlog.push_back(cmd),
        }
    }
}

impl Engine {
    // --- Clock & takeover (device-death safety net) -------------------

    /// The master clock as the video side sees it: normally the audio
    /// clock, wall-clock-extrapolated while that clock is stalled.
    fn playhead(&mut self) -> f64 {
        let clock_pos = self.clock.position();

        if let Some(t) = &self.takeover {
            if clock_pos > t.clock_pos_at_engage + 0.05 {
                // Real forward progress: hand control back.
                self.takeover = None;
                self.last_clock = (clock_pos, Instant::now());
                return clock_pos;
            }
            return t.anchor_pos + t.engaged_at.elapsed().as_secs_f64();
        }

        if self.clock.is_playing() {
            if clock_pos > self.last_clock.0 + 1e-4 {
                self.last_clock = (clock_pos, Instant::now());
            } else if self.last_clock.1.elapsed() > CLOCK_STALL_THRESHOLD {
                self.report(format!(
                    "audio clock stalled at {clock_pos:.3}s — pacing video from the wall clock"
                ));
                self.takeover = Some(Takeover {
                    engaged_at: Instant::now(),
                    anchor_pos: self.position,
                    clock_pos_at_engage: clock_pos,
                });
            }
        }
        match &self.takeover {
            Some(t) => t.anchor_pos + t.engaged_at.elapsed().as_secs_f64(),
            None => clock_pos,
        }
    }

    fn reset_clock_tracking(&mut self) {
        self.takeover = None;
        self.last_clock = (self.clock.position(), Instant::now());
    }

    fn project_fps(&self) -> f64 {
        let fps = self.project.settings.fps;
        if fps.is_finite() && fps > 0.0 {
            fps
        } else {
            30.0
        }
    }

    fn end(&self) -> f64 {
        timeline_end(&self.project)
    }

    fn report(&mut self, msg: String) {
        if self.reported.insert(msg.clone()) {
            (self.sink)(PlayerEvent::Error(msg));
        }
    }

    fn forward_mixer_errors(&mut self) {
        while let Ok(msg) = self.mixer_errors.try_recv() {
            self.report(msg);
        }
    }

    fn emit_position(&mut self) {
        (self.sink)(PlayerEvent::Position {
            position_sec: self.position,
            playing: self.clock.is_playing(),
        });
        self.last_position_event = Instant::now();
    }

    /// (Re)create the GPU renderer when missing or when the preview
    /// resolution changed with the project settings.
    fn ensure_renderer(&mut self) {
        let want = preview_size(&self.project.settings);
        let up_to_date = self
            .renderer
            .as_ref()
            .is_some_and(|r| r.output_size() == want);
        if up_to_date {
            return;
        }
        self.flush_inflight();
        self.cache.clear();
        match TimelineRenderer::new(want.0, want.1, false) {
            Ok(r) => {
                if self.log {
                    eprintln!(
                        "cutty-playback: compositor {}x{} on {}",
                        want.0,
                        want.1,
                        r.adapter_label()
                    );
                }
                self.renderer = Some(r);
            }
            Err(e) => {
                self.renderer = None;
                self.report(format!(
                    "GPU compositor unavailable ({e}) — preview shows black"
                ));
            }
        }
    }

    // --- Command handlers ----------------------------------------------

    fn apply_project(&mut self, project: Project) {
        self.flush_inflight();
        self.project = project;
        self.sources.rebuild(&self.project);
        self.clock
            .set_timeline(mixer_timeline(&self.project, &self.sources));
        // Every cached frame is a composite of the old project.
        self.cache.clear();
        self.in_flight.clear();
        self.ensure_renderer();

        if !self.clock.is_playing() {
            // Live-refresh the paused frame (trims/moves under the
            // playhead show their result immediately).
            self.at_eof = false;
            self.scrub_to(self.position.min(self.end().max(0.0)));
        }
        // While playing, the grid keeps rolling — the next pump resolves
        // against the new snapshot (open decoders stay valid: media files
        // don't change on edits).
    }

    fn do_refresh(&mut self) {
        self.flush_inflight();
        self.sources.rebuild(&self.project);
        self.clock
            .set_timeline(mixer_timeline(&self.project, &self.sources));
        self.cache.clear();
        // Source files may have appeared or been regenerated on disk.
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.clear_sources();
        }
        if !self.clock.is_playing() {
            self.showing_black = false; // re-present even if black before
            self.scrub_to(self.position);
        }
    }

    fn do_play(&mut self) {
        let end = self.end();
        if end <= 0.0 {
            return; // empty timeline: nothing to play
        }
        if self.at_eof || self.position >= end - 1e-9 {
            self.do_seek(0.0);
        }
        self.at_eof = false;
        self.pending_frame = None;
        self.next_grid_idx = None; // re-anchor to the clock
        self.flush_inflight();
        self.clock.play();
        self.reset_clock_tracking();
        self.emit_position();
    }

    fn do_pause(&mut self) {
        self.clock.pause();
        self.reset_clock_tracking();
        self.position = self.playhead().clamp(0.0, self.end().max(0.0));
        self.pending_frame = None;
        self.next_grid_idx = None;
        self.emit_position();
        // Snap the shown frame to the exact pause position (the last
        // presented frame may be one ahead of the clock).
        self.scrub_to(self.position);
        if self.log {
            let r = self.renderer.as_ref().map(|r| r.stats());
            eprintln!(
                "cutty-playback: pause presented={} dropped={} renderer={r:?}",
                self.stats.frames_presented, self.stats.frames_dropped
            );
        }
    }

    fn do_seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.end().max(0.0));
        self.clock.seek(t);
        self.pending_frame = None;
        self.next_grid_idx = None;
        self.at_eof = false;
        self.scrub_to(t);
        self.reset_clock_tracking();
        self.emit_position();
    }

    fn do_step(&mut self, n: i64) {
        if self.clock.is_playing() {
            self.do_pause();
        }
        let fps = self.project_fps();
        let end = self.end();
        let max_frame = ((end * fps).ceil() as i64 - 1).max(0);
        let current = (self.position * fps).round() as i64;
        let target = (current + n).clamp(0, max_frame);
        let t = target as f64 / fps;
        self.clock.seek(t);
        self.at_eof = false;
        self.scrub_to(t);
        self.emit_position();
    }

    // --- Frame production -------------------------------------------------

    /// Produce the composited, encoded output frame for grid index `idx`:
    /// cache first, then the renderer, synchronously (the paused path;
    /// [`Engine::compose_frame_playing`] is the pipelined playing path).
    /// `None` = black (gap, no renderer, or a failed compose — failures
    /// are reported). Callers guarantee no frame is in flight.
    fn compose_frame(&mut self, idx: i64) -> Option<EncodedFrame> {
        debug_assert!(self.inflight.is_none(), "sync compose with a frame in flight");
        let fps = self.project_fps();
        let t = idx as f64 / fps;

        if let Some(hit) = self.cache.get(idx) {
            return Some(EncodedFrame {
                timeline_pts: t,
                width: hit.width,
                height: hit.height,
                jpeg: hit.jpeg.clone(),
            });
        }
        if !self.submit_frame(idx, 0) {
            return None;
        }
        self.read_encoded(idx, 0)
    }

    /// The playing-path frame producer: consumes the pre-submitted frame
    /// when it matches, submits grid frame `idx + 1` to the GPU *before*
    /// reading `idx` back, then reads + JPEG-encodes `idx`. The GPU
    /// renders the next frame while the CPU encodes this one — the same
    /// double buffering the export frontend uses, which is what keeps
    /// transition spans (two decodes + shader passes) inside the frame
    /// budget.
    fn compose_frame_playing(&mut self, idx: i64) -> Option<EncodedFrame> {
        let fps = self.project_fps();
        if let Some(hit) = self.cache.get(idx) {
            let frame = EncodedFrame {
                timeline_pts: idx as f64 / fps,
                width: hit.width,
                height: hit.height,
                jpeg: hit.jpeg.clone(),
            };
            self.flush_inflight();
            return Some(frame);
        }

        let slot = match self.inflight.take() {
            Some(inflight) if inflight.idx == idx => inflight.slot,
            stale => {
                if stale.is_some() {
                    self.inflight = stale;
                    self.flush_inflight();
                }
                if !self.submit_frame(idx, 0) {
                    return None;
                }
                0
            }
        };

        // Pre-submit the next grid frame (if it draws anything and isn't
        // cached) so the GPU works while this frame encodes.
        let next = idx + 1;
        let next_t = next as f64 / fps;
        if next_t < self.end() - 1e-9 && self.cache.get(next).is_none() {
            let next_slot = 1 - slot;
            if self.submit_frame(next, next_slot) {
                self.inflight = Some(InflightFrame {
                    idx: next,
                    slot: next_slot,
                });
            }
        }

        self.read_encoded(idx, slot)
    }

    /// Submit grid frame `idx`'s decode + composite to the GPU on `slot`
    /// (no readback). `false` when nothing renders there (gap, renderer
    /// down, or a failed submit — failures are reported).
    fn submit_frame(&mut self, idx: i64, slot: usize) -> bool {
        let fps = self.project_fps();
        let t = idx as f64 / fps;
        if resolve_video_layers(&self.project, t).is_empty() || self.renderer.is_none() {
            return false;
        }
        let (result, issues) = {
            let Engine {
                renderer,
                sources,
                project,
                ..
            } = self;
            let renderer = renderer.as_mut().expect("checked above");
            let result = renderer.begin_frame(project, t, &|media| sources.video_path(media), slot);
            (result, renderer.take_issues())
        };
        for issue in issues {
            self.report(issue);
        }
        match result {
            Ok(()) => true,
            Err(e) => {
                self.report(format!("compose failed: {e}"));
                false
            }
        }
    }

    /// Block on `slot`'s readback, JPEG-encode it, and cache it as `idx`.
    fn read_encoded(&mut self, idx: i64, slot: usize) -> Option<EncodedFrame> {
        let fps = self.project_fps();
        let t = idx as f64 / fps;
        let (result, (out_w, out_h)) = {
            let Engine { renderer, jpeg, .. } = self;
            let renderer = renderer.as_mut()?;
            let result = renderer.read_frame(slot, |frame| {
                jpeg.encode_rgba_strided(frame.width, frame.height, frame.stride, frame.data)
            });
            (result, renderer.output_size())
        };
        match result {
            Ok(Ok(jpeg)) => {
                self.cache.insert(
                    idx,
                    CachedFrame {
                        width: out_w,
                        height: out_h,
                        jpeg: jpeg.clone(),
                    },
                );
                Some(EncodedFrame {
                    timeline_pts: t,
                    width: out_w,
                    height: out_h,
                    jpeg,
                })
            }
            Ok(Err(e)) => {
                self.report(format!("jpeg encode: {e}"));
                None
            }
            Err(e) => {
                self.report(format!("compose failed: {e}"));
                None
            }
        }
    }

    /// Discard a pre-submitted frame (transport or project changed under
    /// it). The readback must still be consumed — an unread submission
    /// would poison its slot's next map.
    fn flush_inflight(&mut self) {
        if let Some(inflight) = self.inflight.take() {
            if let Some(renderer) = self.renderer.as_mut() {
                let _ = renderer.read_frame(inflight.slot, |_| ());
            }
        }
    }

    /// Show the correct frame for timeline time `t` (paused-path: scrub,
    /// step, seek preview, post-edit refresh). Renders on the output
    /// frame grid, cache-first.
    fn scrub_to(&mut self, t: f64) {
        self.flush_inflight();
        self.position = t;
        let fps = self.project_fps();
        let idx = (t * fps + 1e-6).floor() as i64;
        match self.compose_frame(idx.max(0)) {
            Some(frame) => self.present(frame, t),
            None => self.present_black(t),
        }
    }

    fn present(&mut self, frame: EncodedFrame, position: f64) {
        let clock_sec = self.playhead();
        (self.sink)(PlayerEvent::Frame {
            pts_sec: frame.timeline_pts,
            clock_sec,
            width: frame.width,
            height: frame.height,
            jpeg: frame.jpeg,
        });
        self.position = position;
        self.showing_black = false;
        self.stats.frames_presented += 1;
    }

    /// Present the gap frame (a tiny black JPEG the canvas stretches).
    fn present_black(&mut self, position: f64) {
        self.position = position;
        if self.showing_black {
            return;
        }
        let black = vec![0u8; 16 * 16 * 3];
        match self.jpeg.encode_rgb(16, 16, &black) {
            Ok(jpeg) => {
                let clock_sec = self.playhead();
                (self.sink)(PlayerEvent::Frame {
                    pts_sec: position,
                    clock_sec,
                    width: 16,
                    height: 16,
                    jpeg,
                });
                self.showing_black = true;
            }
            Err(e) => {
                let msg = format!("jpeg encode: {e}");
                self.report(msg);
            }
        }
    }

    // --- Playback pump ---------------------------------------------------

    /// Drive playback for (at most) one output frame. Returns a command
    /// that interrupted a pacing wait, if any.
    fn pump(&mut self, cmd_rx: &Receiver<Cmd>) -> Option<Cmd> {
        let t = self.playhead();
        let end = self.end();

        // End of timeline?
        if t >= end - 1e-9 {
            self.clock.pause();
            self.position = end;
            self.at_eof = true;
            self.pending_frame = None;
            self.next_grid_idx = None;
            self.flush_inflight();
            (self.sink)(PlayerEvent::Eof);
            self.emit_position();
            return None;
        }

        // A frame stashed by an interrupted wait goes first.
        if let Some(frame) = self.pending_frame.take() {
            return self.pace_and_present(frame, cmd_rx);
        }

        // Which grid frame is next? Skip ahead when the render pipeline
        // fell too far behind the clock.
        let fps = self.project_fps();
        let clock_idx = (t * fps + 1e-6).floor() as i64;
        let planned = self.next_grid_idx.unwrap_or(clock_idx).max(0);
        let idx = if (clock_idx - planned) as f64 > DROP_THRESHOLD_FRAMES {
            self.stats.frames_dropped += (clock_idx - planned) as u64;
            if self.log {
                eprintln!(
                    "cutty-playback: dropped {} frames at clock {t:.3}",
                    clock_idx - planned
                );
            }
            clock_idx
        } else {
            planned
        };
        let t_frame = idx as f64 / fps;

        if t_frame >= end - 1e-9 {
            // The grid ran past the last content; idle until the clock
            // reaches the end (the EOF branch fires on the next pump).
            self.next_grid_idx = Some(idx);
            return self.wait_until(end, cmd_rx);
        }

        // Keep decoders warm for what's coming, then render.
        self.maintain_pipeline(t_frame);
        let frame = self.compose_frame_playing(idx);
        self.next_grid_idx = Some(idx + 1);

        match frame {
            Some(frame) => self.pace_and_present(frame, cmd_rx),
            None => {
                // Gap (or renderer failure): black, paced on the grid.
                if let Some(cmd) = self.wait_until(t_frame, cmd_rx) {
                    return Some(cmd);
                }
                self.present_black(t_frame);
                None
            }
        }
    }

    /// Wait until the clock reaches the frame's pts, then present it.
    fn pace_and_present(&mut self, frame: EncodedFrame, cmd_rx: &Receiver<Cmd>) -> Option<Cmd> {
        while self.playhead() + 0.001 < frame.timeline_pts {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    self.pending_frame = Some(frame);
                    return Some(cmd);
                }
                Err(TryRecvError::Disconnected) => return Some(Cmd::Stop),
                Err(TryRecvError::Empty) => std::thread::sleep(CLOCK_POLL),
            }
        }
        let position = frame.timeline_pts;
        self.present(frame, position);
        if self.last_position_event.elapsed() > POSITION_EVENT_INTERVAL {
            self.emit_position();
        }
        None
    }

    /// Keep the decode pipeline ahead of the playhead: install matured
    /// prefetches, request primes for cuts entering the lookahead window,
    /// and close decoders that fell out of range. Transition overlaps
    /// put **both** clips of a pair in the active set, so two streams
    /// stay warm for the whole span.
    fn maintain_pipeline(&mut self, t: f64) {
        if self.renderer.is_none() {
            return;
        }

        // The active stack — every streaming clip with its source time
        // (a transition contributes two).
        let actives: Vec<(u64, u64, f64)> = resolve_track_visuals(&self.project, t)
            .iter()
            .flat_map(|v| match v {
                TrackVisual::Single(c) => vec![c],
                TrackVisual::Transition { from, to, .. } => vec![from, to],
            })
            .filter_map(|a| {
                self.project
                    .find_clip(a.clip_id)
                    .map(|(_, c)| (a.clip_id.0, c.media_id.0, a.source_time))
            })
            .collect();

        // Prime decoders for cuts inside the lookahead window; matured
        // primes install straight into the renderer (`poll_prefetch`),
        // where they wait keyed by clip until their frames arrive.
        let upcoming = upcoming_video_starts(&self.project, t, LOOKAHEAD);
        for up in &upcoming {
            if self.in_flight.contains(&up.clip_id) {
                continue;
            }
            if self
                .renderer
                .as_ref()
                .is_some_and(|r| r.has_session(up.clip_id))
            {
                continue; // already installed (or streaming)
            }
            // A transition target always needs its own session (the
            // outgoing clip keeps streaming beside it); only plain cuts
            // can flow through the running decoder.
            if !up.via_transition && is_continuation(&self.project, up, t) {
                continue;
            }
            let Some(path) = self.sources.video_path(up.media_id) else {
                continue; // proxy pending — the switch will render black
            };
            self.in_flight.insert(up.clip_id);
            self.prefetch.request(PrimeRequest {
                clip_id: up.clip_id,
                media_id: up.media_id,
                path,
                source_in: up.source_in,
            });
        }

        // Reconcile decode sessions when the stack changes: sessions
        // migrate across plain cuts (same media), the rest close.
        let ids: Vec<u64> = actives.iter().map(|&(clip, ..)| clip).collect();
        if ids != self.prev_layer_clips {
            self.prev_layer_clips = ids;
            let needed: Vec<(u64, u64)> = actives
                .iter()
                .map(|&(clip, media, _)| (clip, media))
                .chain(upcoming.iter().map(|u| (u.clip_id, u.media_id)))
                .collect();
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.sync_sources(&needed);
            }
        }
    }

    fn poll_prefetch(&mut self) {
        while let Ok(result) = self.prefetch.res_rx.try_recv() {
            match result {
                Ok(primed) => {
                    // Install immediately: the session waits under its
                    // clip id until that clip's first frame samples it
                    // (the GC keeps upcoming clips). Installing on
                    // activation instead would leave the render path a
                    // cold open when the pipelined pump submits one
                    // frame ahead of the active set.
                    if self.in_flight.remove(&primed.clip_id) {
                        if let Some(renderer) = self.renderer.as_mut() {
                            renderer.offer_decoder(
                                primed.clip_id,
                                primed.media_id,
                                primed.decoder,
                                primed.source_in,
                            );
                        }
                    }
                    // Anything else is stale (project changed): dropped.
                }
                Err((clip_id, msg)) => {
                    if self.in_flight.remove(&clip_id) {
                        self.report(format!("priming next clip failed: {msg}"));
                    }
                }
            }
        }
    }

    /// Sleep (poll-interruptible) until the clock reaches `target`
    /// (strictly — callers rely on the boundary being crossed).
    fn wait_until(&mut self, target: f64, cmd_rx: &Receiver<Cmd>) -> Option<Cmd> {
        while self.playhead() < target {
            if self.last_position_event.elapsed() > POSITION_EVENT_INTERVAL {
                self.position = self.playhead();
                self.emit_position();
            }
            match cmd_rx.try_recv() {
                Ok(cmd) => return Some(cmd),
                Err(TryRecvError::Disconnected) => return Some(Cmd::Stop),
                Err(TryRecvError::Empty) => std::thread::sleep(CLOCK_POLL),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutty_engine::{Engine as EditEngine, ProjectSettings, TrackKind};

    fn project_with(
        clips: &[(f64, f64, f64)], // (timeline_in, source_in, source_out)
        media_dur: f64,
    ) -> Project {
        let mut engine = EditEngine::new(ProjectSettings::default());
        let media = engine
            .add_media("/tmp/does-not-exist.mp4", media_dur, true, true)
            .unwrap();
        let video = engine
            .project()
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .unwrap()
            .id;
        for &(t_in, s_in, s_out) in clips {
            engine.add_clip(video, media, t_in, s_in, s_out).unwrap();
        }
        engine.project().clone()
    }

    #[test]
    fn preview_size_fits_project_aspect_within_720p() {
        let s = |w, h| ProjectSettings {
            width: w,
            height: h,
            fps: 30.0,
        };
        assert_eq!(preview_size(&s(1920, 1080)), (1280, 720));
        assert_eq!(preview_size(&s(3840, 2160)), (1280, 720));
        // Vertical projects fit by height (405 rounds to the even 406).
        assert_eq!(preview_size(&s(1080, 1920)), (406, 720));
        // Small projects never upscale.
        assert_eq!(preview_size(&s(640, 360)), (640, 360));
    }

    #[test]
    fn upcoming_starts_scans_the_lookahead_window() {
        let p = project_with(&[(0.0, 0.0, 1.0), (1.0, 5.0, 6.0), (3.0, 2.0, 3.0)], 10.0);
        let up = upcoming_video_starts(&p, 0.6, 0.5);
        assert_eq!(up.len(), 1);
        assert_eq!(up[0].timeline_in, 1.0);
        assert_eq!(up[0].source_in, 5.0);
        // Nothing within half a second of 1.5.
        assert!(upcoming_video_starts(&p, 1.5, 0.5).is_empty());
        // Hidden tracks contribute nothing; muted (audio-off) ones still do.
        let mut muted = p.clone();
        muted.tracks.iter_mut().for_each(|t| t.muted = true);
        assert_eq!(upcoming_video_starts(&muted, 0.6, 0.5).len(), 1);
        let mut hidden = p.clone();
        hidden.tracks.iter_mut().for_each(|t| t.hidden = true);
        assert!(upcoming_video_starts(&hidden, 0.6, 0.5).is_empty());
    }

    #[test]
    fn continuation_detects_split_points_but_not_jump_cuts() {
        // Split point: [0,1) src [0,1) → [1,2) src [1,2).
        let p = project_with(&[(0.0, 0.0, 1.0), (1.0, 1.0, 2.0)], 10.0);
        let up = &upcoming_video_starts(&p, 0.7, 0.5)[0];
        assert!(is_continuation(&p, up, 0.7));

        // Jump cut: [0,1) src [0,1) → [1,2) src [5,6).
        let p = project_with(&[(0.0, 0.0, 1.0), (1.0, 5.0, 6.0)], 10.0);
        let up = &upcoming_video_starts(&p, 0.7, 0.5)[0];
        assert!(!is_continuation(&p, up, 0.7));
    }

    #[test]
    fn backlog_collapses_seek_project_and_step_runs() {
        let mut backlog: VecDeque<Cmd> = [
            Cmd::Seek(1.0),
            Cmd::Seek(2.0),
            Cmd::Seek(3.0),
            Cmd::Play,
            Cmd::Seek(4.0),
            Cmd::Step(1),
            Cmd::Step(1),
            Cmd::Step(-1),
        ]
        .into_iter()
        .collect();
        collapse_backlog(&mut backlog);
        assert_eq!(backlog.len(), 4);
        assert!(matches!(backlog[0], Cmd::Seek(t) if t == 3.0));
        assert!(matches!(backlog[1], Cmd::Play));
        assert!(matches!(backlog[2], Cmd::Seek(t) if t == 4.0));
        assert!(matches!(backlog[3], Cmd::Step(1)), "steps sum");
    }

    #[test]
    fn mixer_timeline_uses_volume_and_skips_muted_and_missing() {
        let mut p = project_with(&[(0.0, 0.0, 2.0)], 10.0);
        p.tracks[0].clips[0].volume = 0.5;
        let media_id = p.media[0].id.0;
        let mut sources = Sources::default();
        sources.map.insert(
            media_id,
            SourceFiles::Ready {
                proxy: PathBuf::from("/proxy.mp4"),
                has_audio: true,
            },
        );
        let mt = mixer_timeline(&p, &sources);
        assert_eq!(mt.segments.len(), 1);
        assert_eq!(mt.segments[0].volume, 0.5);
        assert_eq!(mt.segments[0].path, PathBuf::from("/proxy.mp4"));

        // Muted track → no segments.
        p.tracks[0].muted = true;
        assert!(mixer_timeline(&p, &sources).segments.is_empty());
        p.tracks[0].muted = false;

        // Hiding a video track removes its picture, not its audio.
        p.tracks[0].hidden = true;
        assert_eq!(mixer_timeline(&p, &sources).segments.len(), 1);
        p.tracks[0].hidden = false;

        // Proxy pending → no segments (silence until refresh).
        sources.map.insert(media_id, SourceFiles::ProxyPending);
        assert!(mixer_timeline(&p, &sources).segments.is_empty());
    }

    #[test]
    fn audio_only_media_resolves_to_original_path() {
        let mut engine = EditEngine::new(ProjectSettings::default());
        let media = engine
            .add_media("/tmp/music.mp3", 30.0, false, true)
            .unwrap();
        let audio_track = engine
            .project()
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Audio)
            .unwrap()
            .id;
        engine.add_clip(audio_track, media, 0.0, 0.0, 5.0).unwrap();
        let p = engine.project().clone();

        let mut sources = Sources::default();
        sources.rebuild(&p);
        assert_eq!(
            sources.audio_path(media.0),
            Some(PathBuf::from("/tmp/music.mp3"))
        );
        assert_eq!(sources.video_path(media.0), None);

        let mt = mixer_timeline(&p, &sources);
        assert_eq!(mt.segments.len(), 1);
        assert_eq!(mt.segments[0].path, PathBuf::from("/tmp/music.mp3"));
    }
}
