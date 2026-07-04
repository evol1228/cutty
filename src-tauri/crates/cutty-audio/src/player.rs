//! Audio playback: symphonia (MP4/AAC) decode thread → SPSC ring buffer →
//! cpal output stream.
//!
//! Design notes:
//! - The cpal stream runs for the whole life of the player; "pause" feeds
//!   silence rather than calling `stream.pause()`, which ALSA plug/dmix/
//!   pipewire-alsa devices frequently do not support.
//! - Seeks are a two-phase handshake: the decoder thread seeks the format
//!   reader and parks the new position on the clock; the audio callback
//!   drains the ring and rebases the counters, so stale audio and the
//!   position flip together.
//! - AAC encoder priming (`track.delay`) is skipped at the start and folded
//!   into seek targets, so position 0 is the first *presentation* sample.

use std::fs::File;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{
    FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType,
};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase, Timestamp};

use crate::clock::PlaybackClock;
use crate::error::AudioError;

/// Ring buffer capacity in seconds. Small enough that a post-seek refill is
/// instant, big enough to ride out scheduling hiccups.
const RING_SECONDS: f64 = 0.25;

/// How long the decoder sleeps when the ring is full.
const FULL_RING_POLL: Duration = Duration::from_millis(5);

enum DecoderCmd {
    Seek(f64),
    Stop,
}

/// Plays one file's audio track and owns the master clock.
pub struct AudioPlayer {
    clock: Arc<PlaybackClock>,
    cmd_tx: Sender<DecoderCmd>,
    decoder_thread: Option<JoinHandle<()>>,
    /// Keeps the device stream alive; dropping it stops audio.
    _stream: cpal::Stream,
    sample_rate: u32,
    channels: usize,
}

impl AudioPlayer {
    /// Open `path`, select its default audio track, and get ready to play
    /// (paused, at position 0).
    pub fn open(path: &Path) -> Result<Self, AudioError> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe().probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or(AudioError::NoAudioTrack)?;
        let track_info = TrackInfo::try_from(track)?;

        let clock = Arc::new(PlaybackClock::new(track_info.sample_rate));
        let ring_cap =
            ((track_info.sample_rate as f64 * RING_SECONDS) as usize) * track_info.channels;
        let (producer, consumer) = HeapRb::<f32>::new(ring_cap.max(1024)).split();

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let stream = build_output_stream(
            track_info.sample_rate,
            track_info.channels,
            consumer,
            clock.clone(),
        )?;
        stream.play()?; // streams never auto-start in cpal 0.18

        let decoder_thread = {
            let clock = clock.clone();
            let info = track_info.clone();
            std::thread::Builder::new()
                .name("cutty-audio-decoder".into())
                .spawn(move || {
                    if let Err(e) = run_decoder(format, info, producer, clock, cmd_rx) {
                        eprintln!("cutty-audio: decoder thread exited with error: {e}");
                    }
                })?
        };

        Ok(Self {
            clock,
            cmd_tx,
            decoder_thread: Some(decoder_thread),
            _stream: stream,
            sample_rate: track_info.sample_rate,
            channels: track_info.channels,
        })
    }

    /// The shared master clock.
    pub fn clock(&self) -> Arc<PlaybackClock> {
        self.clock.clone()
    }

    pub fn play(&self) {
        self.clock.set_playing(true);
    }

    pub fn pause(&self) {
        self.clock.set_playing(false);
    }

    /// Seek to `secs` (presentation time). Returns immediately; the clock
    /// rebases when the seek lands (a few milliseconds).
    pub fn seek(&self, secs: f64) {
        let _ = self.cmd_tx.send(DecoderCmd::Seek(secs.max(0.0)));
    }

    pub fn position_secs(&self) -> f64 {
        self.clock.position_secs()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(DecoderCmd::Stop);
        if let Some(t) = self.decoder_thread.take() {
            let _ = t.join();
        }
    }
}

/// Everything the decoder loop needs to know about the selected track.
#[derive(Clone)]
struct TrackInfo {
    track_id: u32,
    time_base: TimeBase,
    sample_rate: u32,
    channels: usize,
    /// Encoder priming frames (AAC delay). Presentation 0 = track frame
    /// `delay`. symphonia 0.6's isomp4 reader does not apply edit lists, so
    /// we handle it ourselves.
    delay: u64,
}

impl TryFrom<&Track> for TrackInfo {
    type Error = AudioError;

    fn try_from(track: &Track) -> Result<Self, AudioError> {
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or(AudioError::MissingParams("audio codec parameters"))?;
        Ok(TrackInfo {
            track_id: track.id,
            time_base: track
                .time_base
                .ok_or(AudioError::MissingParams("time base"))?,
            sample_rate: params
                .sample_rate
                .ok_or(AudioError::MissingParams("sample rate"))?,
            channels: params
                .channels
                .as_ref()
                .map(|c| c.count())
                .ok_or(AudioError::MissingParams("channel count"))?,
            delay: u64::from(track.delay.unwrap_or(0)),
        })
    }
}

/// Convert a track timestamp to PCM frames. For MP4 audio the timebase is
/// normally 1/sample_rate (ticks == frames), but don't assume it.
fn ts_to_frames(ts: Timestamp, tb: TimeBase, rate: u32) -> u64 {
    let t = ts.get().max(0) as u128;
    (t * u128::from(tb.numer.get()) * u128::from(rate) / u128::from(tb.denom.get())) as u64
}

fn build_output_stream(
    sample_rate: u32,
    channels: usize,
    mut consumer: impl Consumer<Item = f32> + Send + 'static,
    clock: Arc<PlaybackClock>,
) -> Result<cpal::Stream, AudioError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioError::NoDevice)?;

    // cpal never resamples. The ALSA "default" plug layer (dmix or
    // pipewire-alsa on Arch) accepts any sane rate, so requesting the
    // file's rate is the normal path; raw hw devices may reject it.
    let rate_supported = device.supported_output_configs()?.any(|r| {
        r.sample_format() == cpal::SampleFormat::F32
            && usize::from(r.channels()) >= channels
            && r.contains_rate(sample_rate)
    });
    if !rate_supported {
        return Err(AudioError::UnsupportedRate(sample_rate));
    }

    let config = cpal::StreamConfig {
        channels: channels as cpal::ChannelCount,
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };

    let cb_clock = clock.clone();
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            // 1) Apply a pending seek: drop stale audio and rebase, even
            //    while paused (paused seeks must still flush the ring).
            if let Some(base) = cb_clock.take_rebase() {
                consumer.clear();
                cb_clock.apply_rebase(base);
            }

            // 2) Feed the device. Paused ⇒ silence without consuming.
            if cb_clock.is_playing() {
                let n = consumer.pop_slice(data);
                data[n..].fill(0.0);
                cb_clock.advance((n / channels) as u64);
            } else {
                data.fill(0.0);
            }

            // 3) Refresh the latency measurement every callback.
            let ts = info.timestamp();
            let latency = ts.playback.duration_since(ts.callback);
            cb_clock.record_callback(latency.as_nanos() as u64);
        },
        move |err: cpal::Error| {
            // Xrun = underrun; ALSA recovers automatically. Anything else
            // is worth surfacing in the log.
            if !matches!(err.kind(), cpal::ErrorKind::Xrun) {
                eprintln!("cutty-audio: stream error: {err}");
            }
        },
        None,
    )?;
    Ok(stream)
}

/// Decoder loop: demux + decode packets, trim priming/seek remainders, and
/// push interleaved f32 into the ring buffer.
fn run_decoder(
    mut format: Box<dyn FormatReader>,
    info: TrackInfo,
    mut producer: impl Producer<Item = f32>,
    clock: Arc<PlaybackClock>,
    commands: Receiver<DecoderCmd>,
) -> Result<(), SymError> {
    let params = match format
        .tracks()
        .iter()
        .find(|t| t.id == info.track_id)
        .and_then(|t| t.codec_params.as_ref())
        .and_then(|p| p.audio())
    {
        Some(p) => p.clone(),
        None => return Ok(()), // validated in open(); unreachable in practice
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())?;

    let mut interleaved: Vec<f32> = Vec::new();
    // Track frames still to discard: starts with the encoder priming.
    let mut discard_frames: u64 = info.delay;
    let mut at_eof = false;
    // A command that interrupted the push loop, to handle at the top.
    let mut pending_cmd: Option<DecoderCmd> = None;

    'outer: loop {
        // --- 1) Commands. Block when there is nothing left to decode. ---
        let cmd = match pending_cmd.take() {
            Some(c) => Some(c),
            None if at_eof => match commands.recv() {
                Ok(c) => Some(c),
                Err(_) => break 'outer,
            },
            None => match commands.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break 'outer,
            },
        };

        if let Some(cmd) = cmd {
            match cmd {
                DecoderCmd::Stop => break 'outer,
                DecoderCmd::Seek(secs) => {
                    // Presentation secs → track time (fold the priming in).
                    let track_secs =
                        secs + info.delay as f64 / f64::from(info.sample_rate);
                    let Some(time) = Time::try_from_secs_f64(track_secs) else {
                        continue;
                    };
                    match format.seek(
                        SeekMode::Accurate,
                        SeekTo::Time {
                            time,
                            track_id: Some(info.track_id),
                        },
                    ) {
                        Ok(seeked) => {
                            decoder.reset();
                            let req =
                                ts_to_frames(seeked.required_ts, info.time_base, info.sample_rate);
                            let act =
                                ts_to_frames(seeked.actual_ts, info.time_base, info.sample_rate);
                            discard_frames = req.saturating_sub(act);
                            at_eof = false;
                            // Presentation base = required track frames − priming.
                            clock.request_rebase(req.saturating_sub(info.delay));
                        }
                        Err(e) => eprintln!("cutty-audio: seek failed: {e}"),
                    }
                    continue;
                }
            }
        }

        if at_eof {
            continue;
        }

        // --- 2) Demux + decode. ---
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => {
                at_eof = true;
                continue;
            }
            Err(e) => return Err(e),
        };
        if packet.track_id != info.track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            // Recoverable: skip the packet (decoder clears its buffer).
            Err(SymError::DecodeError(e)) => {
                eprintln!("cutty-audio: decode error (skipping packet): {e}");
                continue;
            }
            Err(e) => return Err(e),
        };
        if decoded.frames() == 0 {
            continue;
        }
        decoded.copy_to_vec_interleaved(&mut interleaved);

        // --- 3) Trim priming / post-seek remainder. ---
        let mut samples: &[f32] = &interleaved;
        if discard_frames > 0 {
            let skip = (discard_frames as usize).min(decoded.frames());
            discard_frames -= skip as u64;
            samples = &samples[skip * info.channels..];
        }

        // --- 4) Push into the ring; poll for commands while it's full. ---
        while !samples.is_empty() {
            let n = producer.push_slice(samples);
            samples = &samples[n..];
            if samples.is_empty() {
                break;
            }
            match commands.try_recv() {
                Ok(cmd) => {
                    // A seek/stop invalidates the rest of this packet.
                    pending_cmd = Some(cmd);
                    continue 'outer;
                }
                Err(TryRecvError::Disconnected) => break 'outer,
                Err(TryRecvError::Empty) => std::thread::sleep(FULL_RING_POLL),
            }
        }
    }
    Ok(())
}
