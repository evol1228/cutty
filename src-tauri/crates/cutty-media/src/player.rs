//! The Phase 0 playback engine: one proxy file, audio-master A/V sync.
//!
//! A control thread owns the video decoder and the audio backend. Video
//! frames are decoded ahead, JPEG-encoded, then *presented* (handed to the
//! sink) only when the master clock reaches their timestamp. Late frames
//! are dropped, never the clock adjusted — audio is the master (CLAUDE.md
//! rule 5).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cutty_audio::AudioPlayer;
use serde::Serialize;

use crate::error::MediaError;
use crate::framecache::{CachedFrame, FrameCache};
use crate::jpeg::JpegEncoder;
use crate::probe::probe;
use crate::video::VideoDecoder;

/// Frames later than this behind the clock are dropped instead of shown.
const DROP_THRESHOLD_FRAMES: f64 = 1.5;
/// Poll granularity while waiting for the clock to reach a frame's pts.
const CLOCK_POLL: Duration = Duration::from_millis(2);
/// Cadence of position events while playing.
const POSITION_EVENT_INTERVAL: Duration = Duration::from_millis(250);

/// Everything the UI needs to set up the player view.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_sec: f64,
    pub has_audio: bool,
}

/// Events pushed from the playback engine to the embedder.
pub enum PlayerEvent {
    /// A frame due for presentation *now*.
    Frame {
        pts_sec: f64,
        /// Master-clock reading at presentation. `pts_sec - clock_sec` is
        /// the instantaneous A/V offset (≈0 when in sync).
        clock_sec: f64,
        width: u32,
        height: u32,
        jpeg: Vec<u8>,
    },
    /// Transport/position report.
    Position { position_sec: f64, playing: bool },
    /// Playback reached the end of the file (transport is now paused).
    Eof,
    /// A non-fatal engine error worth surfacing.
    Error(String),
}

pub type EventSink = Box<dyn Fn(PlayerEvent) + Send + 'static>;

enum PlayerCmd {
    Play,
    Pause,
    TogglePlay,
    Seek(f64),
    Step(i64),
    Stop,
}

/// Handle to a running playback engine. Dropping it stops playback and
/// tears down the decoder processes.
pub struct Player {
    cmd_tx: Sender<PlayerCmd>,
    thread: Option<JoinHandle<()>>,
    info: PlayerInfo,
}

impl Player {
    /// Open a proxy file and start the engine (paused, showing frame 0).
    pub fn open(proxy_path: &Path, sink: EventSink) -> Result<Self, MediaError> {
        let media = probe(proxy_path)?;
        let video = media.video.as_ref().ok_or_else(|| MediaError::NoStreams {
            path: proxy_path.display().to_string(),
        })?;
        if video.fps <= 0.0 {
            return Err(MediaError::FfmpegFailed {
                context: Some("opening player".into()),
                message: format!("invalid fps {} in proxy", video.fps),
            });
        }

        let backend = if media.audio.is_some() {
            match AudioPlayer::open(proxy_path) {
                Ok(a) => Backend::Audio(a),
                Err(e) => {
                    // A missing/busy audio device must not kill video
                    // preview; fall back to a wall-clock pace.
                    eprintln!("cutty-media: audio unavailable ({e}); using freewheel clock");
                    Backend::Freewheel(FreewheelClock::default())
                }
            }
        } else {
            Backend::Freewheel(FreewheelClock::default())
        };

        let info = PlayerInfo {
            width: video.width,
            height: video.height,
            fps: video.fps,
            duration_sec: media.duration_sec,
            has_audio: matches!(backend, Backend::Audio(_)),
        };

        let jpeg = JpegEncoder::new()?;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let thread = {
            let info = info.clone();
            let path = proxy_path.to_path_buf();
            std::thread::Builder::new()
                .name("cutty-player".into())
                .spawn(move || run_player(path, info, backend, jpeg, sink, cmd_rx))?
        };

        Ok(Self {
            cmd_tx,
            thread: Some(thread),
            info,
        })
    }

    pub fn info(&self) -> &PlayerInfo {
        &self.info
    }

    pub fn play(&self) {
        let _ = self.cmd_tx.send(PlayerCmd::Play);
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(PlayerCmd::Pause);
    }

    pub fn toggle_play(&self) {
        let _ = self.cmd_tx.send(PlayerCmd::TogglePlay);
    }

    pub fn seek(&self, secs: f64) {
        let _ = self.cmd_tx.send(PlayerCmd::Seek(secs));
    }

    /// Step `delta` frames (negative = backwards). Pauses playback.
    pub fn step(&self, delta: i64) {
        let _ = self.cmd_tx.send(PlayerCmd::Step(delta));
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PlayerCmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// --- Clock backends ---

/// Wall-clock pacing for files with no (usable) audio track.
#[derive(Default)]
struct FreewheelClock {
    state: Mutex<FreewheelState>,
}

#[derive(Default)]
struct FreewheelState {
    base: f64,
    started: Option<Instant>,
}

impl FreewheelClock {
    fn play(&self) {
        let mut s = self.state.lock().expect("freewheel poisoned");
        if s.started.is_none() {
            s.started = Some(Instant::now());
        }
    }

    fn pause(&self) {
        let mut s = self.state.lock().expect("freewheel poisoned");
        if let Some(t) = s.started.take() {
            s.base += t.elapsed().as_secs_f64();
        }
    }

    fn seek(&self, secs: f64) {
        let mut s = self.state.lock().expect("freewheel poisoned");
        s.base = secs;
        if s.started.is_some() {
            s.started = Some(Instant::now());
        }
    }

    fn position(&self) -> f64 {
        let s = self.state.lock().expect("freewheel poisoned");
        s.base + s.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0)
    }

    fn is_playing(&self) -> bool {
        self.state.lock().expect("freewheel poisoned").started.is_some()
    }
}

enum Backend {
    Audio(AudioPlayer),
    Freewheel(FreewheelClock),
}

impl Backend {
    fn play(&self) {
        match self {
            Backend::Audio(a) => a.play(),
            Backend::Freewheel(f) => f.play(),
        }
    }

    fn pause(&self) {
        match self {
            Backend::Audio(a) => a.pause(),
            Backend::Freewheel(f) => f.pause(),
        }
    }

    fn seek(&self, secs: f64) {
        match self {
            Backend::Audio(a) => a.seek(secs),
            Backend::Freewheel(f) => f.seek(secs),
        }
    }

    fn position(&self) -> f64 {
        match self {
            Backend::Audio(a) => a.position_secs(),
            Backend::Freewheel(f) => f.position(),
        }
    }

    fn is_playing(&self) -> bool {
        match self {
            Backend::Audio(a) => a.clock().is_playing(),
            Backend::Freewheel(f) => f.is_playing(),
        }
    }
}

// --- The control loop ---

struct Engine {
    path: PathBuf,
    info: PlayerInfo,
    backend: Backend,
    jpeg: JpegEncoder,
    sink: EventSink,
    video: Option<VideoDecoder>,
    /// Encoded frames already seen — serves seeks/steps into visited
    /// content instantly (cold decode restarts cost ~110 ms).
    cache: FrameCache,
    /// pts of the last presented frame — the UI-visible position while
    /// paused, and the anchor for frame stepping.
    position: f64,
    frame_dur: f64,
    last_position_event: Instant,
}

fn run_player(
    path: PathBuf,
    info: PlayerInfo,
    backend: Backend,
    jpeg: JpegEncoder,
    sink: EventSink,
    cmd_rx: Receiver<PlayerCmd>,
) {
    let frame_dur = 1.0 / info.fps;
    let mut engine = Engine {
        path,
        info,
        backend,
        jpeg,
        sink,
        video: None,
        cache: FrameCache::new(crate::framecache::DEFAULT_CAPACITY_BYTES),
        position: 0.0,
        frame_dur,
        last_position_event: Instant::now(),
    };

    // Show frame 0 immediately so the player never opens black.
    engine.do_seek(0.0);

    let mut pending: Option<PlayerCmd> = None;
    loop {
        let cmd = match pending.take() {
            Some(c) => Some(c),
            None if engine.backend.is_playing() => match cmd_rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            },
            None => match cmd_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };

        match cmd {
            Some(PlayerCmd::Stop) => break,
            Some(PlayerCmd::Play) => engine.do_play(),
            Some(PlayerCmd::Pause) => engine.do_pause(),
            Some(PlayerCmd::TogglePlay) => {
                if engine.backend.is_playing() {
                    engine.do_pause();
                } else {
                    engine.do_play();
                }
            }
            Some(PlayerCmd::Seek(t)) => engine.do_seek(t),
            Some(PlayerCmd::Step(n)) => engine.do_step(n),
            None => {}
        }

        if engine.backend.is_playing() {
            pending = engine.pump_frame(&cmd_rx);
        }
    }
    // Engine drop kills the video decoder process and the audio stream.
}

impl Engine {
    fn emit_position(&mut self) {
        (self.sink)(PlayerEvent::Position {
            position_sec: self.position,
            playing: self.backend.is_playing(),
        });
        self.last_position_event = Instant::now();
    }

    fn restart_video(&mut self, at: f64) -> bool {
        // Kill the old session off-thread: reaping an in-flight ffmpeg
        // (full pipes, reader threads) costs tens of ms we'd otherwise add
        // to seek latency.
        if let Some(old) = self.video.take() {
            std::thread::spawn(move || old.stop());
        }
        match VideoDecoder::open(&self.path, at, self.info.fps) {
            Ok(d) => {
                self.video = Some(d);
                true
            }
            Err(e) => {
                (self.sink)(PlayerEvent::Error(format!("video decode failed: {e}")));
                false
            }
        }
    }

    fn do_play(&mut self) {
        // Play at the end restarts from the top (standard player behavior).
        if self.position >= self.info.duration_sec - self.frame_dur {
            self.do_seek(0.0);
        }
        if self.video.is_none() && !self.restart_video(self.position) {
            return;
        }
        self.backend.play();
        self.emit_position();
    }

    fn do_pause(&mut self) {
        self.backend.pause();
        self.emit_position();
    }

    /// Seek to `t`: rebase the audio clock and present a preview frame
    /// immediately (also while paused) — from the cache when the content
    /// was already visited, otherwise from a fresh decode session.
    fn do_seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.info.duration_sec.max(0.0));
        let was_playing = self.backend.is_playing();
        self.backend.seek(t);

        let frame_index = (t * self.info.fps).round() as i64;
        if !was_playing && self.emit_cached(frame_index) {
            // Cache hit while paused: instant preview; drop the (now
            // mispositioned) decode session and respawn lazily on
            // play/step. Keeps scrubbing free of process spawns.
            self.video = None;
        } else {
            if !self.restart_video(t) {
                return;
            }
            match self.next_frame() {
                Some(frame) => {
                    self.present_frame(frame);
                }
                None => {
                    // Seek landed at/after the last frame.
                    self.position = t;
                }
            }
        }

        if was_playing {
            // The audio rebase is applied by the device callback; wait for
            // it so the pump doesn't mistake the old clock for "late".
            self.wait_for_clock_near(t, Duration::from_millis(300));
        }
        self.emit_position();
    }

    /// Step n frames on the frame grid, pausing playback.
    fn do_step(&mut self, n: i64) {
        if self.backend.is_playing() {
            self.backend.pause();
        }
        let total_frames = (self.info.duration_sec * self.info.fps).round() as i64;
        let current = (self.position * self.info.fps).round() as i64;
        let target = (current + n).clamp(0, (total_frames - 1).max(0));

        if target == current + 1 && self.video.is_some() {
            // Natural forward step: pull the next frame off the live session.
            match self.next_frame() {
                Some(frame) => {
                    self.present_frame(frame);
                    self.backend.seek(self.position);
                    self.emit_position();
                    return;
                }
                None => {
                    self.video = None;
                    (self.sink)(PlayerEvent::Eof);
                }
            }
        }

        // Visited content: serve from cache without touching the decoder.
        if self.emit_cached(target) {
            self.video = None;
            self.backend.seek(self.position);
            self.emit_position();
            return;
        }

        // Backward step / cold session: restart slightly before the target
        // frame so rounding can't skip past it.
        let target_pts = target as f64 / self.info.fps;
        if !self.restart_video((target_pts - 0.3 * self.frame_dur).max(0.0)) {
            return;
        }
        match self.next_frame() {
            Some(frame) => {
                self.present_frame(frame);
                self.backend.seek(self.position);
            }
            None => (self.sink)(PlayerEvent::Eof),
        }
        self.emit_position();
    }

    /// Present a cached frame if `frame_index` was visited. Returns hit.
    fn emit_cached(&mut self, frame_index: i64) -> bool {
        let Some(cached) = self.cache.get(frame_index) else {
            return false;
        };
        let cached = cached.clone();
        (self.sink)(PlayerEvent::Frame {
            pts_sec: cached.pts_sec,
            clock_sec: self.backend.position(),
            width: cached.width,
            height: cached.height,
            jpeg: cached.jpeg,
        });
        self.position = cached.pts_sec;
        true
    }

    fn cache_frame(&mut self, pts: f64, width: u32, height: u32, jpeg: &[u8]) {
        self.cache.insert(
            (pts * self.info.fps).round() as i64,
            CachedFrame {
                pts_sec: pts,
                width,
                height,
                jpeg: jpeg.to_vec(),
            },
        );
    }

    /// Decode, pace against the master clock, and present one frame.
    /// Returns a command that interrupted the pacing wait, if any.
    fn pump_frame(&mut self, cmd_rx: &Receiver<PlayerCmd>) -> Option<PlayerCmd> {
        if self.video.is_none() && !self.restart_video(self.position) {
            self.backend.pause();
            return None;
        }
        let Some(frame) = self.next_frame() else {
            // End of stream: freeze on the last frame, pause the transport.
            self.backend.pause();
            self.video = None;
            (self.sink)(PlayerEvent::Eof);
            self.emit_position();
            return None;
        };
        let pts = self
            .video
            .as_ref()
            .map(|v| v.frame_pts(&frame))
            .unwrap_or(self.position);

        // Late beyond the threshold ⇒ drop and catch up.
        if pts < self.backend.position() - DROP_THRESHOLD_FRAMES * self.frame_dur {
            return None;
        }

        // Encode before the wait so presentation lands on the clock edge.
        let encoded = match self.jpeg.encode_rgb(frame.width, frame.height, &frame.data) {
            Ok(j) => j,
            Err(e) => {
                (self.sink)(PlayerEvent::Error(format!("jpeg encode: {e}")));
                return None;
            }
        };

        // Chase the clock: present when it reaches the frame's pts.
        while self.backend.position() + 0.001 < pts {
            match cmd_rx.try_recv() {
                Ok(cmd) => return Some(cmd),
                Err(TryRecvError::Disconnected) => return Some(PlayerCmd::Stop),
                Err(TryRecvError::Empty) => std::thread::sleep(CLOCK_POLL),
            }
        }

        self.cache_frame(pts, frame.width, frame.height, &encoded);
        (self.sink)(PlayerEvent::Frame {
            pts_sec: pts,
            clock_sec: self.backend.position(),
            width: frame.width,
            height: frame.height,
            jpeg: encoded,
        });
        self.position = pts;

        if self.last_position_event.elapsed() > POSITION_EVENT_INTERVAL {
            self.emit_position();
        }
        None
    }

    fn next_frame(&mut self) -> Option<ffmpeg_sidecar::event::OutputVideoFrame> {
        self.video.as_mut()?.next_frame()
    }

    fn present_frame(&mut self, frame: ffmpeg_sidecar::event::OutputVideoFrame) {
        let pts = self
            .video
            .as_ref()
            .map(|v| v.frame_pts(&frame))
            .unwrap_or(self.position);
        match self.jpeg.encode_rgb(frame.width, frame.height, &frame.data) {
            Ok(jpeg) => {
                self.cache_frame(pts, frame.width, frame.height, &jpeg);
                (self.sink)(PlayerEvent::Frame {
                    pts_sec: pts,
                    clock_sec: self.backend.position(),
                    width: frame.width,
                    height: frame.height,
                    jpeg,
                });
                self.position = pts;
            }
            Err(e) => (self.sink)(PlayerEvent::Error(format!("jpeg encode: {e}"))),
        }
    }

    fn wait_for_clock_near(&self, t: f64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while (self.backend.position() - t).abs() > 0.25 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::generate_video_only_clip;
    use std::sync::mpsc;

    enum Evt {
        Frame(f64),
        #[allow(dead_code)]
        Position(f64, bool),
        Eof,
        #[allow(dead_code)]
        Error(String),
    }

    fn open_collecting(path: &Path) -> (Player, mpsc::Receiver<Evt>) {
        let (tx, rx) = mpsc::channel();
        let player = Player::open(
            path,
            Box::new(move |e| {
                let _ = tx.send(match e {
                    PlayerEvent::Frame { pts_sec, .. } => Evt::Frame(pts_sec),
                    PlayerEvent::Position {
                        position_sec,
                        playing,
                    } => Evt::Position(position_sec, playing),
                    PlayerEvent::Eof => Evt::Eof,
                    PlayerEvent::Error(e) => Evt::Error(e),
                });
            }),
        )
        .unwrap();
        (player, rx)
    }

    #[test]
    fn opens_paused_with_a_preview_frame() {
        let clip = generate_video_only_clip("player-open", 30, 2);
        let (player, rx) = open_collecting(&clip);
        assert!(!player.info().has_audio);
        assert!((player.info().fps - 30.0).abs() < 0.5);

        let first = rx.recv_timeout(Duration::from_secs(5)).ok();
        assert!(
            matches!(first, Some(Evt::Frame(pts)) if pts < 0.05),
            "must show frame 0 on open"
        );
        drop(player); // must not hang
    }

    #[test]
    fn plays_at_realtime_pace_and_reaches_eof() {
        let clip = generate_video_only_clip("player-pace", 30, 2);
        let (player, rx) = open_collecting(&clip);
        let _ = rx.recv_timeout(Duration::from_secs(5)); // preview frame

        player.play();
        let start = Instant::now();
        let mut frames: Vec<f64> = Vec::new();
        let mut saw_eof = false;
        while start.elapsed() < Duration::from_secs(4) {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Evt::Frame(pts)) => frames.push(pts),
                Ok(Evt::Eof) => {
                    saw_eof = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_eof, "2s clip must reach EOF within 4s of play");
        // ~60 frames in 2s; allow slack for startup.
        assert!(frames.len() >= 50, "got {} frames", frames.len());
        assert!(
            frames.windows(2).all(|w| w[1] > w[0]),
            "pts must be strictly increasing"
        );
        // Realtime pace: EOF must not arrive much before the content length.
        assert!(
            start.elapsed() >= Duration::from_millis(1600),
            "played 2s of video in {:?} — not realtime pacing",
            start.elapsed()
        );
    }

    #[test]
    fn seek_shows_preview_frame_while_paused() {
        let clip = generate_video_only_clip("player-seek", 30, 2);
        let (player, rx) = open_collecting(&clip);
        let _ = rx.recv_timeout(Duration::from_secs(5));

        player.seek(1.0);
        let mut preview = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(Evt::Frame(pts)) => {
                    preview = Some(pts);
                    break;
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        let pts = preview.expect("seek must emit a preview frame");
        assert!((0.9..=1.15).contains(&pts), "preview pts {pts}");
    }

    #[test]
    fn frame_stepping_moves_one_frame_on_the_grid() {
        let clip = generate_video_only_clip("player-step", 30, 2);
        let (player, rx) = open_collecting(&clip);
        let first = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Evt::Frame(pts)) => pts,
            _ => panic!("no preview frame"),
        };

        let mut last = first;
        for i in 1..=3 {
            player.step(1);
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut stepped = None;
            while Instant::now() < deadline {
                if let Ok(Evt::Frame(pts)) = rx.recv_timeout(Duration::from_millis(300)) {
                    stepped = Some(pts);
                    break;
                }
            }
            let pts = stepped.unwrap_or_else(|| panic!("step {i} emitted no frame"));
            let delta = pts - last;
            assert!(
                (delta - 1.0 / 30.0).abs() < 0.004,
                "step {i}: delta {delta} not one frame"
            );
            last = pts;
        }

        // And one step back.
        player.step(-1);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut back = None;
        while Instant::now() < deadline {
            if let Ok(Evt::Frame(pts)) = rx.recv_timeout(Duration::from_millis(300)) {
                back = Some(pts);
                break;
            }
        }
        let pts = back.expect("backward step emitted no frame");
        assert!(
            ((last - pts) - 1.0 / 30.0).abs() < 0.004,
            "backstep delta {}",
            last - pts
        );
    }
}
