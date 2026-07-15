//! Timeline playback: plays the project across clips, cuts, and gaps.
//!
//! Replaces the Phase 0 single-file player. The frame transport is
//! unchanged (decode → JPEG → binary IPC → canvas); what drives it is the
//! timeline resolver ([`cutty_engine::resolve`]) chasing the audio master
//! clock:
//!
//! - **Audio is the master clock.** [`cutty_audio::TimelineAudio`] mixes
//!   every audio-contributing clip sample-accurately (gaps = silence, so
//!   the clock never stalls while playing). Video presentation *chases*
//!   that clock — late frames are dropped, the clock is never adjusted.
//! - **One decoder session per active source.** ~[`LOOKAHEAD`] before a
//!   cut, the next segment's decoder is opened and its first frame
//!   decoded + encoded on a prefetch thread, so crossing the cut is a
//!   present-from-memory operation, not a decode. Sessions for sources
//!   out of range are closed. Pure split points (same source, contiguous
//!   ranges) reuse the running session and need no priming at all.
//! - **Gaps** render a black frame and silence; playback continues.
//! - **Scrubbing** while paused coalesces seek requests (only the latest
//!   matters) and serves visited content from the source-keyed
//!   [`FrameCache`]; cold positions pay one in-process seek (<100 ms).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cutty_audio::{AudioSegment, MixerTimeline, TimelineAudio};
use cutty_engine::{active_video_clip, next_boundary_after, timeline_end, Project};

use crate::decode::SourceDecoder;
use crate::error::MediaError;
use crate::framecache::{CachedFrame, FrameCache, DEFAULT_CAPACITY_BYTES};
use crate::jpeg::JpegEncoder;
use crate::proxy::proxy_path_for;

/// Seconds of lookahead before a cut at which the next segment's decoder
/// is opened and primed.
const LOOKAHEAD: f64 = 0.5;
/// Frames later than this behind the clock are dropped instead of shown.
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
// Timeline segments (the video side's view of resolver output)
// ---------------------------------------------------------------------

/// The video clip window the player is inside (or approaching).
#[derive(Debug, Clone, PartialEq)]
struct Segment {
    clip_id: u64,
    media_id: u64,
    timeline_in: f64,
    timeline_out: f64,
    source_in: f64,
    source_out: f64,
    speed: f64,
}

impl Segment {
    fn source_time(&self, t: f64) -> f64 {
        self.source_in + (t - self.timeline_in) * self.speed
    }

    fn timeline_pts(&self, source_pts: f64) -> f64 {
        self.timeline_in + (source_pts - self.source_in) / self.speed
    }
}

/// The video segment visible at `t`, via the resolver.
fn video_segment_at(project: &Project, t: f64) -> Option<Segment> {
    let active = active_video_clip(project, t)?;
    let (_, clip) = project.find_clip(active.clip_id)?;
    Some(Segment {
        clip_id: clip.id.0,
        media_id: clip.media_id.0,
        timeline_in: clip.timeline_in,
        timeline_out: clip.timeline_out,
        source_in: clip.source_in,
        source_out: clip.source_out,
        speed: clip.speed,
    })
}

/// The next video segment to become visible strictly after `t`, and the
/// time it appears. Walks resolver boundaries so runs of gaps and
/// non-video edges collapse.
fn next_video_segment_after(project: &Project, t: f64) -> Option<(f64, Segment)> {
    let current_clip = video_segment_at(project, t).map(|s| s.clip_id);
    let mut probe = t;
    for _ in 0..4096 {
        let boundary = next_boundary_after(project, probe)?;
        if let Some(seg) = video_segment_at(project, boundary) {
            if Some(seg.clip_id) != current_clip {
                return Some((boundary, seg));
            }
        }
        probe = boundary;
    }
    None
}

/// Everything the mixer needs: one [`AudioSegment`] per audio-contributing
/// clip on an unmuted track. Video-track clips contribute their media's
/// audio (scaled by clip volume) from the proxy; audio-track clips play
/// from their resolved file. Muting a track silences it either way.
fn mixer_timeline(project: &Project, sources: &Sources) -> MixerTimeline {
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
            segments.push(AudioSegment {
                path,
                timeline_in: clip.timeline_in,
                timeline_out: clip.timeline_out,
                source_in: clip.source_in,
                speed: clip.speed,
                volume: clip.volume,
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
    /// Source fps, learned from the first decoder session per media and
    /// kept across session closes (needed for cache keys).
    fps: HashMap<u64, f64>,
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
    Freewheel { base: f64, started: Option<Instant> },
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

/// Request to open + position a decoder for an upcoming segment.
struct PrimeRequest {
    clip_id: u64,
    media_id: u64,
    path: PathBuf,
    source_in: f64,
}

/// A decoder positioned on its segment's first frame, with that frame
/// already encoded — crossing the cut is a present-from-memory operation.
struct Primed {
    clip_id: u64,
    media_id: u64,
    decoder: SourceDecoder,
    first_src_pts: f64,
    width: u32,
    height: u32,
    jpeg: Vec<u8>,
}

struct Prefetcher {
    req_tx: Sender<PrimeRequest>,
    res_rx: Receiver<Result<Primed, (u64, String)>>,
    /// clip id of the in-flight request (results for anything else are
    /// stale and dropped).
    in_flight: Option<u64>,
    _thread: JoinHandle<()>,
}

impl Prefetcher {
    fn start() -> Result<Self, MediaError> {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<PrimeRequest>();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("cutty-prefetch".into())
            .spawn(move || {
                let Ok(mut jpeg) = JpegEncoder::new() else {
                    return;
                };
                while let Ok(req) = req_rx.recv() {
                    let result = prime(&req, &mut jpeg).map_err(|e| (req.clip_id, e.to_string()));
                    if res_tx.send(result).is_err() {
                        return;
                    }
                }
            })?;
        Ok(Self {
            req_tx,
            res_rx,
            in_flight: None,
            _thread: thread,
        })
    }

    fn request(&mut self, req: PrimeRequest) {
        self.in_flight = Some(req.clip_id);
        let _ = self.req_tx.send(req);
    }
}

fn prime(req: &PrimeRequest, jpeg: &mut JpegEncoder) -> Result<Primed, MediaError> {
    let mut decoder = SourceDecoder::open(&req.path)?;
    let frame = decoder
        .seek_to(req.source_in)?
        .ok_or_else(|| MediaError::NoStreams {
            path: req.path.display().to_string(),
        })?;
    let encoded = jpeg.encode_rgb_strided(frame.width, frame.height, frame.stride, frame.data)?;
    let (first_src_pts, width, height) = (frame.pts_sec, frame.width, frame.height);
    Ok(Primed {
        clip_id: req.clip_id,
        media_id: req.media_id,
        decoder,
        first_src_pts,
        width,
        height,
        jpeg: encoded,
    })
}

// ---------------------------------------------------------------------
// The control loop
// ---------------------------------------------------------------------

/// An encoded frame held between pump iterations (e.g. when a command
/// interrupted the pacing wait) so it is presented, not lost.
struct EncodedFrame {
    timeline_pts: f64,
    source_pts: f64,
    media_id: u64,
    width: u32,
    height: u32,
    jpeg: Vec<u8>,
}

/// The first frame *past* a segment's out point, decoded while finishing
/// that segment. At a pure split point this exact frame is the next
/// segment's first frame — presenting it from here (remapped) instead of
/// re-decoding keeps split playback gapless and frame-exact.
struct Carryover {
    media_id: u64,
    source_pts: f64,
    width: u32,
    height: u32,
    jpeg: Vec<u8>,
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
    /// Segment switches that found no primed decoder and had to open one
    /// synchronously (a potential visible hitch).
    cold_switches: u64,
    /// Segment switches served from a primed decoder or session reuse.
    warm_switches: u64,
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
    /// Live decode sessions, one per source media in/near range.
    sessions: HashMap<u64, SourceDecoder>,
    prefetch: Prefetcher,
    primed: Option<Primed>,
    /// Segment currently being decoded/presented (while playing).
    active: Option<Segment>,
    /// A decoded-but-not-yet-presented frame (pacing wait interrupted).
    pending_frame: Option<EncodedFrame>,
    /// The frame decoded past the active segment's cut — the incoming
    /// segment's first frame at split points.
    carryover: Option<Carryover>,
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
        sessions: HashMap::new(),
        prefetch,
        primed: None,
        active: None,
        pending_frame: None,
        carryover: None,
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

    // --- Command handlers ----------------------------------------------

    fn apply_project(&mut self, project: Project) {
        self.project = project;
        self.sources.rebuild(&self.project);
        self.clock
            .set_timeline(mixer_timeline(&self.project, &self.sources));
        self.primed = None;
        self.prefetch.in_flight = None;

        if self.clock.is_playing() {
            // Keep rolling: if the active segment survived the edit
            // unchanged the running session is still valid, otherwise
            // re-resolve on the next pump.
            let clock_t = self.playhead();
            let still_valid = self
                .active
                .as_ref()
                .is_some_and(|seg| video_segment_at(&self.project, clock_t).as_ref() == Some(seg));
            if !still_valid {
                self.active = None;
                self.pending_frame = None;
                self.carryover = None;
            }
        } else {
            // Live-refresh the paused frame (trims/moves under the
            // playhead show their result immediately).
            self.at_eof = false;
            self.scrub_to(self.position.min(self.end().max(0.0)));
        }
    }

    fn do_refresh(&mut self) {
        self.sources.rebuild(&self.project);
        self.clock
            .set_timeline(mixer_timeline(&self.project, &self.sources));
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
        self.active = None; // re-resolve at the clock position
        self.pending_frame = None;
        self.carryover = None;
        self.clock.play();
        self.reset_clock_tracking();
        self.emit_position();
    }

    fn do_pause(&mut self) {
        self.clock.pause();
        self.reset_clock_tracking();
        self.position = self.playhead().clamp(0.0, self.end().max(0.0));
        self.pending_frame = None;
        self.carryover = None;
        self.active = None;
        self.emit_position();
        // Snap the shown frame to the exact pause position (the last
        // presented frame may be one ahead of the clock).
        self.scrub_to(self.position);
    }

    fn do_seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.end().max(0.0));
        self.clock.seek(t);
        self.pending_frame = None;
        self.carryover = None;
        self.active = None;
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

    // --- Frame presentation ---------------------------------------------

    /// Show the correct frame for timeline time `t` (paused-path: scrub,
    /// step, seek preview, post-edit refresh). Cache-first; cold
    /// positions do one in-process seek.
    fn scrub_to(&mut self, t: f64) {
        self.position = t;
        let Some(seg) = video_segment_at(&self.project, t) else {
            self.present_black(t);
            return;
        };
        let source_t = seg.source_time(t);

        // Cache hit? (Needs the source fps to compute the frame key.)
        if let Some(&fps) = self.sources.fps.get(&seg.media_id) {
            let idx = (source_t * fps + 1e-6).floor() as i64;
            if let Some(hit) = self.cache.get((seg.media_id, idx)) {
                let frame = EncodedFrame {
                    timeline_pts: seg.timeline_pts(hit.source_pts_sec),
                    source_pts: hit.source_pts_sec,
                    media_id: seg.media_id,
                    width: hit.width,
                    height: hit.height,
                    jpeg: hit.jpeg.clone(),
                };
                self.present(frame, t);
                return;
            }
        }

        // Cold: position a real session (kept warm for subsequent steps).
        if self.session_for(&seg).is_none() {
            self.present_black(t);
            return;
        }
        // Field-split borrows: `sessions` and `jpeg` are disjoint.
        let session = self.sessions.get_mut(&seg.media_id).expect("session_for");
        let result = match session.seek_to(source_t) {
            Ok(Some(frame)) => Ok(Some((
                seg.timeline_pts(frame.pts_sec),
                frame.pts_sec,
                frame.width,
                frame.height,
                self.jpeg
                    .encode_rgb_strided(frame.width, frame.height, frame.stride, frame.data),
            ))),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        };
        match result {
            Ok(Some((timeline_pts, source_pts, width, height, Ok(jpeg)))) => {
                self.present(
                    EncodedFrame {
                        timeline_pts,
                        source_pts,
                        media_id: seg.media_id,
                        width,
                        height,
                        jpeg,
                    },
                    t,
                );
            }
            Ok(Some((.., Err(e)))) => {
                let msg = format!("jpeg encode: {e}");
                self.report(msg);
            }
            Ok(None) => self.present_black(t),
            Err(e) => {
                let msg = format!("decode failed: {e}");
                self.sessions.remove(&seg.media_id);
                self.report(msg);
                self.present_black(t);
            }
        }
    }

    /// Get (or open) the decode session for a segment's source.
    fn session_for(&mut self, seg: &Segment) -> Option<&mut SourceDecoder> {
        if !self.sessions.contains_key(&seg.media_id) {
            let path = match self.sources.video_path(seg.media_id) {
                Some(p) => p,
                None => {
                    let msg = match self.sources.map.get(&seg.media_id) {
                        Some(SourceFiles::ProxyPending) => {
                            "preview not ready yet — proxy still generating".to_string()
                        }
                        _ => format!("no playable video for media {}", seg.media_id),
                    };
                    self.report(msg);
                    return None;
                }
            };
            match SourceDecoder::open(&path) {
                Ok(session) => {
                    self.sources.fps.insert(seg.media_id, session.fps());
                    self.sessions.insert(seg.media_id, session);
                }
                Err(e) => {
                    self.report(format!("open {} failed: {e}", path.display()));
                    return None;
                }
            }
        }
        self.sessions.get_mut(&seg.media_id)
    }

    fn present(&mut self, frame: EncodedFrame, position: f64) {
        // Source-keyed cache insert: scrubs back into this content are
        // then instant.
        if let Some(&fps) = self.sources.fps.get(&frame.media_id) {
            let idx = (frame.source_pts * fps + 1e-6).round() as i64;
            self.cache.insert(
                (frame.media_id, idx),
                CachedFrame {
                    source_pts_sec: frame.source_pts,
                    width: frame.width,
                    height: frame.height,
                    jpeg: frame.jpeg.clone(),
                },
            );
        }
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

    /// Drive playback for (at most) one frame. Returns a command that
    /// interrupted a pacing wait, if any.
    fn pump(&mut self, cmd_rx: &Receiver<Cmd>) -> Option<Cmd> {
        let t = self.playhead();

        // End of timeline?
        if t >= self.end() - 1e-9 {
            self.clock.pause();
            self.position = self.end();
            self.at_eof = true;
            self.active = None;
            self.pending_frame = None;
            (self.sink)(PlayerEvent::Eof);
            self.emit_position();
            return None;
        }

        // A frame stashed by an interrupted wait goes first.
        if let Some(frame) = self.pending_frame.take() {
            return self.pace_and_present(frame, cmd_rx);
        }

        // (Re-)resolve the active segment when the clock left it.
        let needs_switch = match &self.active {
            Some(seg) => !(seg.timeline_in <= t && t < seg.timeline_out),
            None => true,
        };
        if needs_switch {
            match video_segment_at(&self.project, t) {
                Some(seg) => {
                    if let Some(cmd) = self.switch_to(seg, cmd_rx) {
                        return Some(cmd);
                    }
                }
                None => return self.idle_through_gap(t, cmd_rx),
            }
            return None; // switched (or failed): next pump decodes
        }

        let seg = self.active.clone()?;

        // Kick lookahead priming for the upcoming segment.
        self.plan_lookahead(&seg, t);

        // Steady state: decode → encode → pace → present.
        if self.session_for(&seg).is_none() {
            // Proxy missing / decoder broken: show black through this
            // segment, pace forward to its end.
            self.present_black(t);
            return self.wait_until(seg.timeline_out, cmd_rx);
        }

        enum Decoded {
            Frame(EncodedFrame),
            PastCut(Option<Carryover>),
            SourceDry,
            Failed(String),
        }

        let frame_dur = 1.0 / self.sources.fps.get(&seg.media_id).copied().unwrap_or(30.0);
        let late_horizon = self.playhead() - DROP_THRESHOLD_FRAMES * frame_dur;

        let decoded = {
            let session = self.sessions.get_mut(&seg.media_id).expect("session_for");
            match session.next_frame() {
                Ok(Some(frame)) => {
                    let src_pts = frame.pts_sec;
                    if src_pts >= seg.source_out - CONTINUITY_EPS {
                        // The frame belongs past the cut — never present
                        // it *here*. Keep it: at a split point it is the
                        // incoming clip's first frame.
                        let carry = self
                            .jpeg
                            .encode_rgb_strided(frame.width, frame.height, frame.stride, frame.data)
                            .ok()
                            .map(|jpeg| Carryover {
                                media_id: seg.media_id,
                                source_pts: src_pts,
                                width: frame.width,
                                height: frame.height,
                                jpeg,
                            });
                        Decoded::PastCut(carry)
                    } else {
                        let timeline_pts = seg.timeline_pts(src_pts);
                        if timeline_pts < late_horizon {
                            Decoded::Frame(EncodedFrame {
                                timeline_pts,
                                source_pts: src_pts,
                                media_id: seg.media_id,
                                width: 0, // marker: dropped below
                                height: 0,
                                jpeg: Vec::new(),
                            })
                        } else {
                            // Encode before the pacing wait so
                            // presentation lands on the clock edge.
                            match self.jpeg.encode_rgb_strided(
                                frame.width,
                                frame.height,
                                frame.stride,
                                frame.data,
                            ) {
                                Ok(jpeg) => Decoded::Frame(EncodedFrame {
                                    timeline_pts,
                                    source_pts: src_pts,
                                    media_id: seg.media_id,
                                    width: frame.width,
                                    height: frame.height,
                                    jpeg,
                                }),
                                Err(e) => Decoded::Failed(format!("jpeg encode: {e}")),
                            }
                        }
                    }
                }
                Ok(None) => Decoded::SourceDry,
                Err(e) => Decoded::Failed(format!("decode failed: {e}")),
            }
        };

        match decoded {
            Decoded::Frame(frame) => {
                if frame.width == 0 {
                    // Late beyond the threshold ⇒ dropped; catch up now.
                    self.stats.frames_dropped += 1;
                    if self.log {
                        eprintln!(
                            "cutty-playback: drop t={:.3} clock={:.3}",
                            frame.timeline_pts,
                            self.clock.position()
                        );
                    }
                    None
                } else {
                    self.pace_and_present(frame, cmd_rx)
                }
            }
            Decoded::PastCut(carry) => {
                // This segment is fully presented. Hold until the clock
                // actually crosses the cut, then transition — without
                // this the next pump re-resolves *inside* the old
                // segment and churns cold re-seeks until the boundary.
                self.carryover = carry;
                self.active = None;
                self.wait_until(seg.timeline_out + 1e-4, cmd_rx)
            }
            Decoded::SourceDry => {
                // Source ran out before source_out (short proxy): black
                // to the end of the segment.
                self.active = None;
                self.present_black(t);
                self.wait_until(seg.timeline_out, cmd_rx)
            }
            Decoded::Failed(msg) => {
                self.sessions.remove(&seg.media_id);
                self.report(msg);
                self.active = None;
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

    /// Enter `seg`: present the carried-over frame at a split point, use
    /// the primed decoder at a cut, fall back to the running session when
    /// it's already positioned, or (worst case) open one synchronously.
    fn switch_to(&mut self, seg: Segment, cmd_rx: &Receiver<Cmd>) -> Option<Cmd> {
        // We usually get here slightly early (the previous segment's
        // frames ran out); hold until the clock actually crosses the cut.
        if self.playhead() < seg.timeline_in {
            if let Some(cmd) = self.wait_until(seg.timeline_in, cmd_rx) {
                return Some(cmd);
            }
        }

        // Split point: the frame decoded past the previous cut is this
        // segment's first frame — present it, session flows on.
        if let Some(carry) = self.carryover.take() {
            let frame_dur = 1.0 / self.sources.fps.get(&carry.media_id).copied().unwrap_or(30.0);
            if carry.media_id == seg.media_id
                && (carry.source_pts - seg.source_in).abs() < 0.5 * frame_dur
                && self.sessions.contains_key(&seg.media_id)
            {
                let frame = EncodedFrame {
                    timeline_pts: seg.timeline_pts(carry.source_pts),
                    source_pts: carry.source_pts,
                    media_id: carry.media_id,
                    width: carry.width,
                    height: carry.height,
                    jpeg: carry.jpeg,
                };
                self.stats.warm_switches += 1;
                self.log_switch(&seg, "continuous");
                self.active = Some(seg.clone());
                self.gc_sessions(&seg);
                return self.pace_and_present(frame, cmd_rx);
            }
            // Not a continuation (hard cut): the carryover is garbage.
        }

        // Primed cut: install the prefetched decoder and present its
        // already-encoded first frame. Only a *matching* prime is
        // consumed — one for a later boundary must survive this switch.
        if self
            .primed
            .as_ref()
            .is_some_and(|p| p.clip_id == seg.clip_id)
        {
            let primed = self.primed.take().expect("checked above");
            self.sources
                .fps
                .insert(primed.media_id, primed.decoder.fps());
            self.sessions.insert(primed.media_id, primed.decoder);
            let frame = EncodedFrame {
                timeline_pts: seg.timeline_pts(primed.first_src_pts),
                source_pts: primed.first_src_pts,
                media_id: primed.media_id,
                width: primed.width,
                height: primed.height,
                jpeg: primed.jpeg,
            };
            self.stats.warm_switches += 1;
            self.log_switch(&seg, "primed");
            self.active = Some(seg.clone());
            self.gc_sessions(&seg);
            return self.pace_and_present(frame, cmd_rx);
        }

        // No prime. If the session is already positioned where playback
        // needs it (play-after-pause, seek-then-play, boundary races),
        // just keep decoding — no seek.
        let wanted = seg.source_time(self.playhead().max(seg.timeline_in));
        let fps = self.sources.fps.get(&seg.media_id).copied().unwrap_or(30.0);
        let positioned = self
            .sessions
            .get(&seg.media_id)
            .and_then(|s| s.next_pts_hint())
            .is_some_and(|hint| wanted > hint - 1.5 / fps && wanted < hint + 0.5 / fps);

        if positioned {
            self.stats.warm_switches += 1;
            self.log_switch(&seg, "positioned");
            self.active = Some(seg.clone());
            self.gc_sessions(&seg);
            return None;
        }

        // Cold switch: open + position synchronously and present the
        // frame the seek lands on (it *is* the segment's first frame).
        // This path is what lookahead exists to avoid; counted so tests
        // can assert it stays rare.
        self.stats.cold_switches += 1;
        self.log_switch(&seg, "COLD");
        self.active = Some(seg.clone());
        if self.session_for(&seg).is_none() {
            self.gc_sessions(&seg);
            return None; // proxy pending / broken: pump renders black
        }
        let session = self.sessions.get_mut(&seg.media_id).expect("session_for");
        let sought = match session.seek_to(wanted) {
            Ok(Some(frame)) => Some((
                frame.pts_sec,
                frame.width,
                frame.height,
                self.jpeg
                    .encode_rgb_strided(frame.width, frame.height, frame.stride, frame.data),
            )),
            Ok(None) => None,
            Err(e) => {
                self.sessions.remove(&seg.media_id);
                let msg = format!("decode failed: {e}");
                self.report(msg);
                None
            }
        };
        self.gc_sessions(&seg);
        if let Some((source_pts, width, height, Ok(jpeg))) = sought {
            let frame = EncodedFrame {
                timeline_pts: seg.timeline_pts(source_pts),
                source_pts,
                media_id: seg.media_id,
                width,
                height,
                jpeg,
            };
            return self.pace_and_present(frame, cmd_rx);
        }
        None
    }

    /// Close sessions for sources that are neither active nor upcoming
    /// ("one decoder session per active source").
    fn gc_sessions(&mut self, current: &Segment) {
        let mut keep: HashSet<u64> = HashSet::new();
        keep.insert(current.media_id);
        if let Some((_, next)) =
            next_video_segment_after(&self.project, current.timeline_out - 1e-9)
        {
            keep.insert(next.media_id);
        }
        if let Some(p) = &self.primed {
            keep.insert(p.media_id);
        }
        self.sessions.retain(|media_id, _| keep.contains(media_id));
    }

    /// While inside `seg`, make sure the next segment's decoder gets
    /// primed when its start enters the lookahead window.
    fn plan_lookahead(&mut self, seg: &Segment, t: f64) {
        if t + LOOKAHEAD < seg.timeline_out {
            return; // cut not in sight yet
        }
        let Some((_, next)) = next_video_segment_after(&self.project, seg.timeline_out - 1e-9)
        else {
            return; // nothing after this segment
        };
        // Split-point continuations need no priming.
        if next.media_id == seg.media_id
            && (seg.source_out - next.source_in).abs() < CONTINUITY_EPS
            && (seg.timeline_out - next.timeline_in).abs() < CONTINUITY_EPS
        {
            return;
        }
        self.request_prime(&next);
    }

    fn request_prime(&mut self, next: &Segment) {
        if self.prefetch.in_flight == Some(next.clip_id)
            || self
                .primed
                .as_ref()
                .is_some_and(|p| p.clip_id == next.clip_id)
        {
            return; // already on it
        }
        let Some(path) = self.sources.video_path(next.media_id) else {
            return; // proxy pending — the switch will render black
        };
        self.prefetch.request(PrimeRequest {
            clip_id: next.clip_id,
            media_id: next.media_id,
            path,
            source_in: next.source_in,
        });
    }

    fn poll_prefetch(&mut self) {
        while let Ok(result) = self.prefetch.res_rx.try_recv() {
            match result {
                Ok(primed) => {
                    if self.prefetch.in_flight == Some(primed.clip_id) {
                        self.prefetch.in_flight = None;
                        self.primed = Some(primed);
                    }
                    // Anything else is stale (project changed): dropped.
                }
                Err((clip_id, msg)) => {
                    if self.prefetch.in_flight == Some(clip_id) {
                        self.prefetch.in_flight = None;
                    }
                    self.report(format!("priming next clip failed: {msg}"));
                }
            }
        }
    }

    /// Present black and pace through a gap until its end (or a command).
    fn idle_through_gap(&mut self, t: f64, cmd_rx: &Receiver<Cmd>) -> Option<Cmd> {
        self.present_black(t);
        if let Some((start, next)) = next_video_segment_after(&self.project, t) {
            // Prime what comes next while the gap plays out.
            if start - t <= LOOKAHEAD {
                self.request_prime(&next);
            }
            // Wake at the segment start, but poll prefetch results and
            // position events along the way.
            let wake = start.min(t + 0.1);
            let interrupted = self.wait_until(wake, cmd_rx);
            self.poll_prefetch();
            interrupted
        } else {
            // No more video ever — idle towards the timeline end (audio
            // may still be playing under the black).
            self.wait_until((t + 0.1).min(self.end()), cmd_rx)
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

    fn log_switch(&self, seg: &Segment, kind: &str) {
        if self.log {
            eprintln!(
                "cutty-playback: switch[{kind}] clip={} t_in={:.3} clock={:.3} drops={} cold={} warm={}",
                seg.clip_id,
                seg.timeline_in,
                self.clock.position(),
                self.stats.frames_dropped,
                self.stats.cold_switches,
                self.stats.warm_switches,
            );
        }
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
    fn video_segment_lookup_and_mapping() {
        let p = project_with(&[(1.0, 2.0, 5.0)], 10.0);
        assert!(video_segment_at(&p, 0.5).is_none());
        let seg = video_segment_at(&p, 2.0).unwrap();
        assert_eq!(seg.timeline_in, 1.0);
        assert_eq!(seg.source_time(2.0), 3.0);
        assert_eq!(seg.timeline_pts(3.0), 2.0);
    }

    #[test]
    fn next_segment_walks_gaps_and_cuts() {
        let p = project_with(&[(1.0, 0.0, 2.0), (3.0, 4.0, 6.0), (5.0, 0.0, 1.0)], 10.0);
        // From inside the first clip: the next segment starts at 3.
        let (at, seg) = next_video_segment_after(&p, 1.5).unwrap();
        assert_eq!(at, 3.0);
        assert_eq!(seg.timeline_in, 3.0);
        // From the gap before clip 1.
        let (at, seg) = next_video_segment_after(&p, 0.0).unwrap();
        assert_eq!(at, 1.0);
        assert_eq!(seg.timeline_in, 1.0);
        // Touching clips: from inside clip 2 the next is clip 3 at 5.
        let (at, seg) = next_video_segment_after(&p, 4.0).unwrap();
        assert_eq!(at, 5.0);
        assert_eq!(seg.timeline_in, 5.0);
        // Past everything.
        assert!(next_video_segment_after(&p, 6.0).is_none());
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
