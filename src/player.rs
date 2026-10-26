use crate::models::{PlayerState, RepeatMode, Track};
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use rand::seq::SliceRandom;
use rodio::source::UniformSourceIterator;
use rodio::{Decoder, Source};
use std::collections::VecDeque;
use std::fs::File;
use std::io::BufReader;
use std::num::NonZero;
#[cfg(any(test, feature = "test-support"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

type AudioSource = UniformSourceIterator<Decoder<BufReader<File>>>;

#[derive(Default)]
struct Playback {
    queue: Arc<[Track]>,
    order: Vec<Track>,
    index: usize,
    state: PlayerState,
    volume: f64,
    /// Volume to restore when unmuting; kept updated by every audible change.
    last_volume: f64,
    shuffle: bool,
    repeat: RepeatMode,
    duration: f64,
    current: Option<Track>,
}

#[derive(Default)]
struct AudioBuffer {
    samples: VecDeque<f32>,
    played_samples: u64,
    base_position: f64,
}

impl AudioBuffer {
    fn reset(&mut self, base_position: f64) {
        self.samples.clear();
        self.played_samples = 0;
        self.base_position = base_position;
    }
}

struct Shared {
    playback: Mutex<Playback>,
    audio: Mutex<AudioBuffer>,
    listeners: Mutex<Vec<async_channel::Sender<()>>>,
    /// Loudness meter (f32 bits, 0.0..=1.0) of the most recent output block,
    /// written by the audio callback and read by the visual effects.
    level: AtomicU32,
    channels: usize,
    sample_rate: u32,
}

impl Shared {
    fn changed(&self) {
        self.listeners.lock().retain(|listener| {
            !matches!(
                listener.try_send(()),
                Err(async_channel::TrySendError::Closed(_))
            )
        });
    }
}

enum Command {
    Load(Track),
    Seek(f64),
    Clear,
    Shutdown,
}

pub struct Player {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
    _stream: Option<Mutex<cpal::Stream>>,
    #[cfg(any(test, feature = "test-support"))]
    test_driver: Option<JoinHandle<()>>,
    #[cfg(any(test, feature = "test-support"))]
    test_running: Arc<AtomicBool>,
}

fn following(index: usize, len: usize, repeat: RepeatMode, manual: bool) -> Option<usize> {
    if len == 0 {
        None
    } else if repeat == RepeatMode::One && !manual {
        Some(index)
    } else if index + 1 < len {
        Some(index + 1)
    } else if repeat == RepeatMode::Off {
        None
    } else {
        Some(0)
    }
}

fn edit(queue: &mut Arc<[Track]>, change: impl FnOnce(&mut Vec<Track>)) {
    let mut tracks = queue.to_vec();
    change(&mut tracks);
    *queue = tracks.into();
}

fn select(playback: &mut Playback, index: usize) -> Option<Track> {
    playback.index = index;
    let track = playback.queue.get(index)?.clone();
    playback.duration = track.duration;
    playback.current = Some(track.clone());
    playback.state = PlayerState::Playing;
    Some(track)
}

fn insert(playback: &mut Playback, tracks: Vec<Track>, next: bool) {
    let (at, order_at) = if next {
        let current = playback.current.as_ref().map(|c| c.path.clone());
        let order_at = playback
            .order
            .iter()
            .position(|t| Some(&t.path) == current.as_ref())
            .map_or(playback.order.len(), |i| i + 1);
        (playback.index + 1, order_at)
    } else {
        (playback.queue.len(), playback.order.len())
    };
    let at = at.min(playback.queue.len());
    edit(&mut playback.queue, |queue| {
        queue.splice(at..at, tracks.iter().cloned());
    });
    playback.order.splice(order_at..order_at, tracks);
}

fn remove(playback: &mut Playback, indices: &[usize]) {
    let mut indices = indices.to_vec();
    indices.sort_unstable_by(|a, b| b.cmp(a));
    indices.dedup();
    let mut queue = playback.queue.to_vec();
    for i in indices {
        if i == playback.index || i >= queue.len() {
            continue;
        }
        let track = queue.remove(i);
        if i < playback.index {
            playback.index -= 1;
        }
        if let Some(pos) = playback.order.iter().position(|t| t.path == track.path) {
            playback.order.remove(pos);
        }
    }
    playback.queue = queue.into();
}

fn stop(playback: &mut Playback) {
    playback.state = PlayerState::Stopped;
    playback.current = None;
    playback.duration = 0.0;
}

/// Peak meter for the visual effects: instant attack, smooth decay, so the
/// bars follow transients closely and sink back gently between them.
fn envelope(prev: f32, peak: f32) -> f32 {
    if peak > prev {
        peak
    } else {
        (prev * 0.92).max(peak)
    }
}

fn fill_output<T: cpal::SizedSample + cpal::FromSample<f32>>(data: &mut [T], shared: &Shared) {
    let (playing, volume) = {
        let state = shared.playback.lock();
        (state.state == PlayerState::Playing, state.volume as f32)
    };
    let mut audio = shared.audio.lock();
    let mut peak = 0f32;
    for sample in data {
        let value = match playing.then(|| audio.samples.pop_front()).flatten() {
            Some(value) => {
                audio.played_samples += 1;
                value * volume
            }
            None => 0.0,
        };
        let value = value.clamp(-1.0, 1.0);
        peak = peak.max(value.abs());
        *sample = T::from_sample(value);
    }
    let prev = f32::from_bits(shared.level.load(Ordering::Relaxed));
    shared
        .level
        .store(envelope(prev, peak).to_bits(), Ordering::Relaxed);
}

fn build_output<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: Arc<Shared>,
) -> Result<cpal::Stream> {
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], _| fill_output(data, &shared),
        |err| eprintln!("audio output: {err}"),
        None,
    )?)
}

fn open_source(track: &Track, channels: usize, rate: u32) -> Result<(AudioSource, f64)> {
    let file = File::open(&track.path).with_context(|| format!("open {}", track.path.display()))?;
    let decoder =
        Decoder::try_from(file).with_context(|| format!("decode {}", track.path.display()))?;
    let duration = decoder
        .total_duration()
        .map_or(track.duration, |d| d.as_secs_f64());
    let channels = NonZero::new(channels as u16).context("zero output channels")?;
    let rate = NonZero::new(rate).context("zero output sample rate")?;
    Ok((
        UniformSourceIterator::new(decoder, channels, rate),
        duration,
    ))
}

fn load_source(shared: &Shared, track: Track) -> Option<AudioSource> {
    shared.audio.lock().reset(0.0);
    match open_source(&track, shared.channels, shared.sample_rate) {
        Ok((source, duration)) => {
            shared.playback.lock().duration = duration;
            Some(source)
        }
        Err(err) => {
            eprintln!("playback: {err:#}");
            stop(&mut shared.playback.lock());
            None
        }
    }
}

fn apply_command(
    command: Command,
    shared: &Shared,
    source: &mut Option<AudioSource>,
    end_of_file: &mut bool,
) -> bool {
    match command {
        Command::Load(track) => {
            *source = load_source(shared, track);
            *end_of_file = false;
        }
        Command::Seek(secs) => {
            if let Some(source) = source.as_mut() {
                if let Err(err) = source.try_seek(Duration::from_secs_f64(secs)) {
                    eprintln!("seek: {err}");
                } else {
                    shared.audio.lock().reset(secs);
                    *end_of_file = false;
                }
            }
        }
        Command::Clear => {
            *source = None;
            *end_of_file = false;
            shared.audio.lock().reset(0.0);
        }
        Command::Shutdown => return false,
    }
    shared.changed();
    true
}

fn run_decoder(shared: Arc<Shared>, commands: Receiver<Command>) {
    let mut source: Option<AudioSource> = None;
    let mut end_of_file = false;
    loop {
        while let Ok(command) = commands.try_recv() {
            if !apply_command(command, &shared, &mut source, &mut end_of_file) {
                return;
            }
        }

        if let Some(active) = source.as_mut() {
            let buffered = shared.audio.lock().samples.len();
            if !end_of_file && buffered < shared.sample_rate as usize * shared.channels {
                let mut block = Vec::with_capacity(4096);
                for _ in 0..4096 {
                    if let Some(sample) = active.next() {
                        block.push(sample);
                    } else {
                        end_of_file = true;
                        break;
                    }
                }
                shared.audio.lock().samples.extend(block);
                continue;
            }
            if end_of_file && buffered == 0 {
                let next = {
                    let mut playback = shared.playback.lock();
                    let next =
                        following(playback.index, playback.queue.len(), playback.repeat, false);
                    if let Some(index) = next {
                        select(&mut playback, index)
                    } else {
                        stop(&mut playback);
                        None
                    }
                };
                source = next.and_then(|track| load_source(&shared, track));
                end_of_file = false;
                shared.changed();
                continue;
            }
        }
        let command = if source.is_none() {
            commands
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            commands.recv_timeout(Duration::from_millis(10))
        };
        match command {
            Ok(command) => {
                if !apply_command(command, &shared, &mut source, &mut end_of_file) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

impl Player {
    pub fn new(volume: f64) -> Result<Self> {
        let mut hosts = Vec::new();
        for id in [cpal::default_host().id()]
            .into_iter()
            .chain(cpal::available_hosts())
        {
            if !hosts.contains(&id) {
                hosts.push(id);
            }
        }
        let mut failures = Vec::new();
        for id in hosts {
            let Ok(host) = cpal::host_from_id(id) else {
                continue;
            };
            let Some(device) = host.default_output_device() else {
                failures.push(format!("{id}: no default output device"));
                continue;
            };
            match Self::from_device(&device, id, volume) {
                Ok(player) => return Ok(player),
                Err(err) => failures.push(format!("{id}: {err:#}")),
            }
        }
        anyhow::bail!("no usable audio output: {}", failures.join("; "))
    }

    fn from_device(device: &cpal::Device, _host: cpal::HostId, volume: f64) -> Result<Self> {
        let mut supported = device
            .default_output_config()
            .context("query audio output")?;
        if !matches!(
            supported.sample_format(),
            cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16
        ) {
            supported = device
                .supported_output_configs()?
                .find(|config| {
                    config.sample_format() == cpal::SampleFormat::F32
                        && config.contains_rate(48_000)
                })
                .context("no supported PCM audio format")?
                .with_sample_rate(48_000);
        }
        let config = supported.config();
        // PulseAudio-specific buffer size handling is not applicable on macOS (CoreAudio).
        if config.channels == 0 || config.sample_rate == 0 {
            anyhow::bail!("audio device reported an invalid stream configuration");
        }
        let mut player = Self::create(config.channels, config.sample_rate, volume);
        let shared = player.shared.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build_output::<f32>(device, config, shared)?,
            cpal::SampleFormat::I16 => build_output::<i16>(device, config, shared)?,
            cpal::SampleFormat::U16 => build_output::<u16>(device, config, shared)?,
            _ => anyhow::bail!(
                "unsupported audio sample format: {:?}",
                supported.sample_format()
            ),
        };
        stream.play().context("start audio output")?;
        player._stream = Some(Mutex::new(stream));
        Ok(player)
    }

    fn create(channels: u16, sample_rate: u32, volume: f64) -> Self {
        let shared = Arc::new(Shared {
            playback: Mutex::new(Playback {
                volume: volume.clamp(0.0, 1.0),
                last_volume: if volume > 0.0 {
                    volume.clamp(0.0, 1.0)
                } else {
                    1.0
                },
                ..Default::default()
            }),
            audio: Mutex::new(AudioBuffer::default()),
            listeners: Mutex::new(Vec::new()),
            level: AtomicU32::new(0),
            channels: channels as usize,
            sample_rate,
        });
        let (commands, receiver) = mpsc::channel();
        let worker_shared = shared.clone();
        let worker = std::thread::spawn(move || run_decoder(worker_shared, receiver));
        Self {
            shared,
            commands,
            worker: Some(worker),
            _stream: None,
            #[cfg(any(test, feature = "test-support"))]
            test_driver: None,
            #[cfg(any(test, feature = "test-support"))]
            test_running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn play(&self, tracks: Vec<Track>, at: usize) {
        if tracks.is_empty() {
            return;
        }
        let track = {
            let mut p = self.shared.playback.lock();
            p.order = tracks.clone();
            let mut queue = tracks;
            let start = at.min(queue.len() - 1);
            let index = if p.shuffle {
                let first = queue.remove(start);
                queue.shuffle(&mut rand::thread_rng());
                queue.insert(0, first);
                0
            } else {
                start
            };
            p.queue = queue.into();
            select(&mut p, index)
        };
        self.load(track);
        self.shared.changed();
    }

    pub fn subscribe(&self) -> async_channel::Receiver<()> {
        let (sender, receiver) = async_channel::bounded(1);
        self.shared.listeners.lock().push(sender);
        receiver
    }

    pub fn play_next(&self, tracks: Vec<Track>) {
        self.insert(tracks, true);
    }

    pub fn enqueue(&self, tracks: Vec<Track>) {
        self.insert(tracks, false);
    }

    fn insert(&self, tracks: Vec<Track>, next: bool) {
        if tracks.is_empty() {
            return;
        }
        let mut p = self.shared.playback.lock();
        if p.current.is_none() {
            drop(p);
            self.play(tracks, 0);
            return;
        }
        insert(&mut p, tracks, next);
        drop(p);
        self.shared.changed();
    }

    pub fn remove(&self, indices: &[usize]) {
        remove(&mut self.shared.playback.lock(), indices);
        self.shared.changed();
    }

    pub fn jump(&self, index: usize) {
        let track = select(&mut self.shared.playback.lock(), index);
        self.load(track);
        self.shared.changed();
    }

    pub fn queue(&self) -> (Arc<[Track]>, Option<usize>) {
        let p = self.shared.playback.lock();
        (p.queue.clone(), p.current.is_some().then_some(p.index))
    }

    fn load(&self, track: Option<Track>) -> bool {
        let Some(track) = track else {
            return false;
        };
        let _ = self.commands.send(Command::Load(track));
        true
    }

    pub fn toggle(&self) {
        if self.state() == PlayerState::Playing {
            self.pause();
        } else {
            self.resume();
        }
    }

    pub fn resume(&self) {
        let mut p = self.shared.playback.lock();
        if p.current.is_some() {
            p.state = PlayerState::Playing;
        }
        drop(p);
        self.shared.changed();
    }

    pub fn pause(&self) {
        let mut p = self.shared.playback.lock();
        if p.current.is_some() {
            p.state = PlayerState::Paused;
        }
        drop(p);
        self.shared.changed();
    }

    pub fn stop(&self) {
        let mut p = self.shared.playback.lock();
        stop(&mut p);
        p.queue = Arc::new([]);
        p.order.clear();
        let _ = self.commands.send(Command::Clear);
        drop(p);
        self.shared.changed();
    }

    pub fn next(&self) {
        let track = {
            let mut p = self.shared.playback.lock();
            let next = following(p.index, p.queue.len(), p.repeat, true);
            next.and_then(|index| select(&mut p, index))
        };
        if !self.load(track) {
            self.stop();
        }
        self.shared.changed();
    }

    pub fn previous(&self) {
        let track = {
            let mut p = self.shared.playback.lock();
            let position = self.position();
            if position < 3.0 && p.index > 0 {
                let index = p.index - 1;
                select(&mut p, index)
            } else {
                None
            }
        };
        if !self.load(track) {
            self.seek(0.0);
        }
        self.shared.changed();
    }

    pub fn seek(&self, secs: f64) {
        if self.current().is_some() && secs.is_finite() {
            let secs = secs.max(0.0);
            let _ = self.commands.send(Command::Seek(secs));
        }
    }

    pub fn set_volume(&self, volume: f64) {
        let volume = volume.clamp(0.0, 1.0);
        {
            let mut playback = self.shared.playback.lock();
            if volume > 0.0 {
                playback.last_volume = volume;
            }
            playback.volume = volume;
        }
        self.shared.changed();
    }

    pub fn volume(&self) -> f64 {
        self.shared.playback.lock().volume
    }

    pub fn is_muted(&self) -> bool {
        self.volume() <= 0.0
    }

    /// Output loudness in 0.0..=1.0 for the visual effects.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed)).clamp(0.0, 1.0)
    }

    /// Silence playback or restore the previous volume. Returns the muted state.
    pub fn toggle_mute(&self) -> bool {
        let muted = {
            let mut playback = self.shared.playback.lock();
            if playback.volume > 0.0 {
                playback.last_volume = playback.volume;
                playback.volume = 0.0;
                true
            } else {
                playback.volume = if playback.last_volume > 0.0 {
                    playback.last_volume
                } else {
                    1.0
                };
                false
            }
        };
        self.shared.changed();
        muted
    }
    pub fn set_shuffle(&self, enabled: bool) {
        let mut p = self.shared.playback.lock();
        p.shuffle = enabled;
        if let Some(current) = p.current.clone() {
            if enabled {
                edit(&mut p.queue, |queue| {
                    queue.retain(|track| track.path != current.path);
                    queue.shuffle(&mut rand::thread_rng());
                    queue.insert(0, current);
                });
                p.index = 0;
            } else {
                p.queue = p.order.as_slice().into();
                p.index = p
                    .order
                    .iter()
                    .position(|track| track.path == current.path)
                    .unwrap_or(0);
            }
        }
        drop(p);
        self.shared.changed();
    }
    pub fn shuffle(&self) -> bool {
        self.shared.playback.lock().shuffle
    }
    pub fn set_repeat(&self, mode: RepeatMode) {
        self.shared.playback.lock().repeat = mode;
        self.shared.changed();
    }
    pub fn repeat(&self) -> RepeatMode {
        self.shared.playback.lock().repeat
    }
    pub fn state(&self) -> PlayerState {
        self.shared.playback.lock().state
    }
    pub fn position(&self) -> f64 {
        let audio = self.shared.audio.lock();
        audio.base_position
            + audio.played_samples as f64
                / (self.shared.sample_rate as f64 * self.shared.channels as f64)
    }
    pub fn duration(&self) -> f64 {
        self.shared.playback.lock().duration
    }
    pub fn current(&self) -> Option<Track> {
        self.shared.playback.lock().current.clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            self.test_running.store(false, Ordering::Relaxed);
            if let Some(driver) = self.test_driver.take() {
                let _ = driver.join();
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Player {
    pub fn new_test() -> Self {
        let mut player = Self::create(2, 48_000, 1.0);
        player.test_running.store(true, Ordering::Relaxed);
        let shared = player.shared.clone();
        let running = player.test_running.clone();
        player.test_driver = Some(std::thread::spawn(move || {
            // Pace by elapsed time so that a slow sleep on a busy CI runner does not slow playback.
            let samples_per_second = (shared.sample_rate as usize * shared.channels) as f64;
            let start = std::time::Instant::now();
            let mut block = vec![0.0f32; 960];
            let mut consumed = 0usize;
            while running.load(Ordering::Relaxed) {
                let due = (start.elapsed().as_secs_f64() * samples_per_second) as usize;
                while consumed < due {
                    let len = block.len().min(due - consumed);
                    fill_output(&mut block[..len], &shared);
                    consumed += len;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }));
        player
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn track(path: std::path::PathBuf) -> Track {
        Track {
            path,
            title: "Test".into(),
            artist: String::new(),
            album: String::new(),
            track_number: 0,
            year: 0,
            duration: 0.0,
            album_id: String::new(),
        }
    }

    fn named(name: &str) -> Track {
        track(std::path::PathBuf::from(name))
    }

    fn names(tracks: &[Track]) -> Vec<String> {
        tracks
            .iter()
            .map(|t| t.path.to_string_lossy().into_owned())
            .collect()
    }

    fn playing(queue: &[&str], order: &[&str], index: usize) -> Playback {
        let queue: Vec<Track> = queue.iter().map(|n| named(n)).collect();
        Playback {
            current: queue.get(index).cloned(),
            order: order.iter().map(|n| named(n)).collect(),
            queue: queue.into(),
            index,
            ..Default::default()
        }
    }

    #[test]
    fn play_next_inserts_after_current_in_queue_and_order() {
        let mut p = playing(&["c", "a", "b"], &["a", "b", "c"], 0);
        insert(&mut p, vec![named("x"), named("y")], true);
        assert_eq!(names(&p.queue), ["c", "x", "y", "a", "b"]);
        assert_eq!(names(&p.order), ["a", "b", "c", "x", "y"]);
        insert(&mut p, vec![named("z")], false);
        assert_eq!(names(&p.queue), ["c", "x", "y", "a", "b", "z"]);
        assert_eq!(names(&p.order), ["a", "b", "c", "x", "y", "z"]);
    }

    #[test]
    fn remove_keeps_current_and_shifts_index() {
        let mut p = playing(&["a", "b", "c", "d"], &["a", "b", "c", "d"], 2);
        remove(&mut p, &[0, 2, 3]);
        assert_eq!(names(&p.queue), ["b", "c"]);
        assert_eq!(names(&p.order), ["b", "c"]);
        assert_eq!(p.index, 1);
    }

    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        condition()
    }

    fn write_wav(path: &std::path::Path, sample: impl Fn(usize) -> i16) {
        let samples = 44_100u32;
        let mut wav = Vec::with_capacity(44 + samples as usize * 2);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + samples * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&samples.to_le_bytes());
        wav.extend_from_slice(&(samples * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples * 2).to_le_bytes());
        for i in 0..samples as usize {
            wav.extend_from_slice(&sample(i).to_le_bytes());
        }
        std::fs::write(path, wav).unwrap();
    }

    fn write_silent_wav(path: &std::path::Path) {
        write_wav(path, |_| 0);
    }

    #[test]
    fn envelope_attacks_on_peaks_and_decays_in_silence() {
        assert_eq!(envelope(0.0, 0.0), 0.0);
        assert_eq!(
            envelope(0.3, 0.9),
            0.9,
            "transients are picked up instantly"
        );
        let decayed = envelope(0.9, 0.0);
        assert!(decayed < 0.9 && decayed > 0.8, "gentle decay: {decayed}");
        assert!(
            envelope(decayed, 0.0) < decayed,
            "decay continues towards silence"
        );
    }

    #[test]
    fn level_meter_follows_output_amplitude() {
        let base = std::env::temp_dir().join(format!("omamusic-level-{}", std::process::id()));
        let silent = base.join("silent.wav");
        let loud = base.join("loud.wav");
        std::fs::create_dir_all(&base).unwrap();
        write_wav(&silent, |_| 0);
        write_wav(&loud, |i| if i % 2 == 0 { 12_000 } else { -12_000 });

        let player = Player::new_test();
        assert_eq!(player.level(), 0.0, "fresh player reads silent");

        player.play(vec![track(silent.clone())], 0);
        assert!(
            wait_for(|| player.position() > 0.05),
            "silent WAV did not start"
        );
        assert!(
            player.level() < 0.01,
            "silence must not register as loud: {}",
            player.level()
        );

        player.play(vec![track(loud.clone())], 0);
        assert!(
            wait_for(|| player.level() > 0.1),
            "square wave did not raise the level (got {})",
            player.level()
        );
        assert!(player.level() <= 1.0);

        // Muting silences the meter too, since it measures the real output.
        player.toggle_mute();
        assert!(
            wait_for(|| player.level() < 0.02),
            "muted output did not sink (got {})",
            player.level()
        );

        drop(player);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn wav_playback_controls_and_eof() {
        let path = std::env::temp_dir().join(format!("omamusic-test-{}.wav", std::process::id()));
        write_silent_wav(&path);

        let player = Player::new_test();
        player.play(vec![track(path.clone())], 0);
        assert!(
            wait_for(|| player.position() > 0.05),
            "WAV position did not advance"
        );
        assert!(player.duration() > 0.9);
        player.pause();
        assert_eq!(player.state(), PlayerState::Paused);
        player.resume();
        assert_eq!(player.state(), PlayerState::Playing);
        assert!(
            wait_for(|| player.state() == PlayerState::Stopped),
            "WAV did not finish"
        );
        drop(player);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn mp3_with_id3_decodes_without_plugins_or_external_player() {
        let path =
            std::env::temp_dir().join(format!("omamusic-id3-test-{}.mp3", std::process::id()));
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-metadata",
                "title=ID3 test",
                "-metadata",
                "artist=Test artist",
                "-id3v2_version",
                "3",
                "-y",
            ])
            .arg(&path)
            .status()
            .expect("ffmpeg is required only to create the test fixture");
        assert!(status.success());
        let metadata = crate::metadata::read(&path);
        assert_eq!(metadata.title, "ID3 test");
        let player = Player::new_test();
        player.play(vec![track(path.clone())], 0);
        assert!(
            wait_for(|| player.position() > 0.05),
            "MP3 position did not advance"
        );
        drop(player);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "requires a running desktop audio service"]
    fn system_audio_output_plays_file() {
        let path =
            std::env::temp_dir().join(format!("omamusic-device-test-{}.wav", std::process::id()));
        write_silent_wav(&path);
        let player = Player::new(1.0).unwrap();
        player.play(vec![track(path.clone())], 0);
        assert!(
            wait_for(|| player.position() > 0.05),
            "device did not consume audio"
        );
        drop(player);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn toggle_mute_silences_and_restores_the_previous_volume() {
        let player = Player::new_test();
        player.set_volume(0.4);
        assert!(player.toggle_mute(), "clicking while audible mutes");
        assert!(player.is_muted());
        assert_eq!(player.volume(), 0.0);
        assert!(!player.toggle_mute(), "clicking while muted restores");
        assert_eq!(player.volume(), 0.4);

        // Dragging the slider to zero reads as muted; clicking the icon then
        // restores the last audible volume instead of staying silent.
        player.set_volume(0.0);
        assert!(player.is_muted());
        player.toggle_mute();
        assert_eq!(player.volume(), 0.4);
        assert!(!player.is_muted());
        player.set_volume(0.25);
        player.toggle_mute();
        player.toggle_mute();
        assert_eq!(player.volume(), 0.25);
        drop(player);
    }

    #[test]
    fn muting_at_startup_restores_a_usable_volume() {
        let player = Player::create(2, 48_000, 0.0);
        player.toggle_mute();
        assert_eq!(player.volume(), 1.0);
        player.toggle_mute();
        assert_eq!(player.volume(), 0.0);
        drop(player);
    }
}
