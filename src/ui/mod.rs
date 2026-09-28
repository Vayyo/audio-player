//! Native UI and live output stream. Only the short transport operations share a
//! mutex with the audio callback; decoding and conversion run outside that lock.

#[cfg(target_os = "linux")]
mod tray;
mod visualizer;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, atomic::{AtomicBool, Ordering}, mpsc};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use eframe::egui;
use symphonia::core::audio::sample::SampleFormat as PcmSampleFormat;

use crate::core::convert::{DeviceFormat, PcmBuffer, convert};
use crate::core::decode::decode_file;
use crate::core::queue::{MusicQueue, RepeatMode};
use crate::core::player::{PlaybackState, Player};
use self::visualizer::{AsciiDonut, ColorMode};

const CANVAS: egui::Color32 = egui::Color32::from_rgb(4, 4, 4);
const FRAME: egui::Color32 = egui::Color32::from_rgb(12, 12, 12);
const MUTED: egui::Color32 = egui::Color32::from_rgb(150, 150, 150);
const OUTLINE: egui::Color32 = egui::Color32::from_rgb(95, 95, 95);

fn transport(player: &Mutex<Player>) -> MutexGuard<'_, Player> {
    player.lock().unwrap_or_else(|poison| poison.into_inner())
}

// egui's consume_key deliberately accepts extra Alt/Shift and only checks the
// logical key. Match exact modifiers and either logical or physical key so
// shortcuts also work on non-Latin layouts without stealing unrelated keys.
fn shortcut(ctx: &egui::Context, modifiers: egui::Modifiers, key: egui::Key) -> bool {
    ctx.input_mut(|input| {
        if !input.focused {
            return false;
        }
        let index = input.events.iter().position(|event| matches!(
            event,
            egui::Event::Key {
                key: logical, physical_key: physical, modifiers: held, pressed: true, ..
            } if (*logical == key || *physical == Some(key)) && held.matches_exact(modifiers)
        ));
        if let Some(index) = index {
            input.events.remove(index);
            true
        } else {
            false
        }
    })
}

fn change_volume(player: &Mutex<Player>, steps: f32) -> Option<f32> {
    let mut player = transport(player);
    let previous = player.volume();
    player.set_volume((previous + steps * 0.05).clamp(0.0, 1.0));
    let volume = player.volume();
    (volume != previous).then_some(volume)
}

fn seek_track(player: &Mutex<Player>, ended: &AtomicBool, target: u64) {
    let mut player = transport(player);
    player.seek(target);
    ended.store(false, Ordering::Release);
}

fn wheel_notches(ctx: &egui::Context) -> f32 {
    ctx.input(|input| {
        if !input.focused || !input.pointer.hover_pos().is_some_and(|pos| input.viewport_rect().contains(pos)) {
            return 0.0;
        }
        input.events.iter().filter_map(|event| {
            if let egui::Event::MouseWheel { unit, delta, modifiers, .. } = event
                && *modifiers == egui::Modifiers::NONE
            {
                Some(delta.y / if *unit == egui::MouseWheelUnit::Point { 24.0 } else { 1.0 })
            } else {
                None
            }
        }).sum()
    })
}

/// Callback-local gain: one step per output frame, shared by all its channels.
struct GainRamp {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
    frames: u32,
}

impl GainRamp {
    fn new(initial: f32, sample_rate: u32) -> Self {
        Self { current: initial, target: initial, step: 0.0, remaining: 0, frames: (sample_rate / 200).max(1) }
    }

    fn set_target(&mut self, target: f32) {
        if target != self.target {
            self.target = target;
            self.remaining = self.frames;
            self.step = (target - self.current) / self.frames as f32;
        }
    }

    fn next(&mut self) -> f32 {
        if self.remaining != 0 {
            self.remaining -= 1;
            self.current = if self.remaining == 0 { self.target } else { self.current + self.step };
        }
        self.current
    }
}

/// Fill a device buffer from owned, already converted PCM. There is no I/O,
/// allocation, or UI work on this path; the mutex is held only for one output
/// buffer, so position and samples always refer to the same transport state.
fn render<T: SizedSample + FromSample<f32>>(
    output: &mut [T],
    channels: usize,
    samples: &[f32],
    shared: &Mutex<Player>,
    ended: &AtomicBool,
    gain: &mut GainRamp,
) {
    output.fill(T::from_sample(0.0));
    let mut player = transport(shared);
    if player.state() != PlaybackState::Playing || channels == 0 {
        return;
    }
    let available = player.total_frames().saturating_sub(player.position()) as usize;
    if available == 0 {
        // Completion belongs to this track, not to a manual pause or stop.
        player.stop();
        ended.store(true, Ordering::Release);
        return;
    }
    let frames = available.min(output.len() / channels);
    let start = player.position() as usize * channels;
    let end = start + frames * channels;
    gain.set_target(player.volume());
    for (dst_frame, src_frame) in output[..frames * channels]
        .chunks_exact_mut(channels)
        .zip(samples[start..end].chunks_exact(channels))
    {
        let frame_gain = gain.next();
        for (dst, &src) in dst_frame.iter_mut().zip(src_frame) {
            let scaled = src * frame_gain;
            *dst = T::from_sample(if scaled.is_finite() { scaled.clamp(-1.0, 1.0) } else { 0.0 });
        }
    }
    player.advance(frames as u64);
}

struct Track {
    title: String,
    artist: Option<String>,
    rate: u32,
    channels: usize,
    samples: Arc<Vec<f32>>,
    shared: Arc<Mutex<Player>>,
    ended: Arc<AtomicBool>,
    stream: cpal::Stream,
    errors: mpsc::Receiver<String>,
}

fn stream_error_message(err: &cpal::Error) -> Option<String> {
    // CPAL's output worker prepares and refills the stream after an xrun.
    (err.kind() != cpal::ErrorKind::Xrun).then(|| format!("Output stream failed: {err}"))
}

impl Track {
    // The device stays on the UI thread. Decoding and conversion have already
    // finished by the time the stream is constructed.
    fn from_prepared(
        path: &Path,
        device: cpal::Device,
        supported: cpal::SupportedStreamConfig,
        prepared: Prepared,
        volume: f32,
    ) -> Result<Self, String> {
        let Prepared { output, artist } = prepared;
        let target_rate = supported.sample_rate();
        let config = supported.config();
        let mut player = Player::new(output.frames());
        player.set_volume(volume);
        let mut gain = GainRamp::new(player.volume(), target_rate);
        let shared = Arc::new(Mutex::new(player));
        let ended = Arc::new(AtomicBool::new(false));
        let (error_sender, errors) = mpsc::channel();
        let error_callback = move |err: cpal::Error| {
            if let Some(message) = stream_error_message(&err) {
                let _ = error_sender.send(message);
            }
        };
        let callback_player = Arc::clone(&shared);
        let samples = Arc::new(output.samples);
        let callback_samples = Arc::clone(&samples);
        let callback_ended = Arc::clone(&ended);
        let channels = usize::from(supported.channels());
        let stream = match supported.sample_format() {
            SampleFormat::F32 => device.build_output_stream(
                config,
                move |data: &mut [f32], _| render(data, channels, callback_samples.as_slice(), &callback_player, &callback_ended, &mut gain),
                error_callback,
                None,
            ),
            SampleFormat::I16 => device.build_output_stream(
                config,
                move |data: &mut [i16], _| render(data, channels, callback_samples.as_slice(), &callback_player, &callback_ended, &mut gain),
                error_callback,
                None,
            ),
            SampleFormat::U16 => device.build_output_stream(
                config,
                move |data: &mut [u16], _| render(data, channels, callback_samples.as_slice(), &callback_player, &callback_ended, &mut gain),
                error_callback,
                None,
            ),
            _ => unreachable!("validated above"),
        }
        .map_err(|err| format!("Cannot create output stream: {err}"))?;
        let title = path.file_stem().unwrap_or(path.as_os_str()).to_string_lossy().into_owned();
        Ok(Self { title, artist, rate: target_rate, channels, samples, shared, ended, stream, errors })
    }

    fn play(&self) -> Result<(), String> {
        self.stream.play().map_err(|err| format!("Cannot start output stream: {err}"))?;
        let mut player = transport(&self.shared);
        self.ended.store(false, Ordering::Release);
        player.play();
        Ok(())
    }
}

struct Prepared {
    output: PcmBuffer,
    artist: Option<String>,
}

fn prepare(path: &Path, target: DeviceFormat) -> Result<Prepared, String> {
    let decoded = decode_file(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let artist = decoded.artist;
    let source = PcmBuffer::new(decoded.rate, decoded.channels, decoded.samples);
    let output = if source.rate == target.rate && source.channels == target.channels {
        source
    } else {
        convert(&source, &target).map_err(|err| format!("PCM conversion: {err}"))?
    };
    if output.frames() == 0 {
        return Err("File contains no playable output frames".to_owned());
    }
    Ok(Prepared { output, artist })
}

enum PendingOpen {
    Selecting(mpsc::Receiver<Option<PathBuf>>),
    SelectingFolder(mpsc::Receiver<Result<Option<MusicQueue>, String>>),
    Loading {
        path: PathBuf,
        queue: Option<MusicQueue>,
        index: usize,
        autoplay: bool,
        device: cpal::Device,
        supported: cpal::SupportedStreamConfig,
        receiver: mpsc::Receiver<Result<Prepared, String>>,
    },
}

fn poll_worker<T>(receiver: &mpsc::Receiver<T>) -> Option<Result<T, String>> {
    match receiver.try_recv() {
        Ok(result) => Some(Ok(result)),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => {
            Some(Err("File open worker stopped unexpectedly".to_owned()))
        }
    }
}


/// A slider interaction keeps its own target while playback advances underneath.
#[derive(Default)]
struct SeekPreview {
    target: Option<u64>,
}

impl SeekPreview {
    fn displayed(&self, position: u64) -> u64 {
        self.target.unwrap_or(position)
    }

    /// Pointer edits preview until release; keyboard edits commit immediately.
    fn update(&mut self, frame: u64, changed: bool, held: bool, released: bool) -> Option<u64> {
        if held {
            self.target = Some(frame);
            return None;
        }
        let target = if released && !changed { self.target } else { changed.then_some(frame) };
        self.target = None;
        target
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarPage {
    Menu,
    Colour,
    Music,
}

pub struct AudioApp {
    track: Option<Track>,
    queue: Option<MusicQueue>,
    repeat: RepeatMode,
    pending: Option<PendingOpen>,
    error: Option<String>,
    seek_preview: SeekPreview,
    donut: AsciiDonut,
    last_frame: Option<Instant>,
    volume_status: Option<(String, Instant)>,
    sidebar_expanded: bool,
    sidebar_width: f32,
    color_mode: ColorMode,
    sidebar_page: SidebarPage,
    hue: f32,
    wheel_remainder: f32,
    #[cfg(target_os = "linux")]
    tray: Option<tray::TrayService>,
}

impl AudioApp {
    pub fn new(initial: Option<PathBuf>) -> Self {
        let mut app = Self {
            track: None,
            queue: None,
            repeat: RepeatMode::Off,
            pending: None,
            error: None,
            seek_preview: SeekPreview::default(),
            donut: AsciiDonut::default(),
            last_frame: None,
            volume_status: None,
            sidebar_expanded: false,
            sidebar_width: 18.0,
            color_mode: ColorMode::BlackWhite,
            sidebar_page: SidebarPage::Menu,
            hue: 0.0,
            wheel_remainder: 0.0,
            #[cfg(target_os = "linux")]
            tray: None,
        };
        if let Some(path) = initial {
            app.start_load(path);
        }
        app
    }

    #[cfg(target_os = "linux")]
    pub fn start_tray(&mut self, ctx: &egui::Context) {
        self.tray = Some(tray::TrayService::start(ctx));
    }

    fn start_picker(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new().name("audio-file-picker".to_owned()).spawn(move || {
            // rfd's default Linux backend uses the XDG desktop portal, including
            // under Wayland. The synchronous portal call runs only on this worker.
            let chosen = rfd::FileDialog::new()
                .add_filter("Audio", &["mp3", "flac", "wav", "ogg"])
                .pick_file();
            let _ = sender.send(chosen);
        }) {
            Ok(_) => {
                self.pending = Some(PendingOpen::Selecting(receiver));
                self.error = None;
            }
            Err(err) => self.error = Some(format!("Cannot start file picker: {err}")),
        }
    }

    fn start_folder_picker(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new().name("music-folder-picker".to_owned()).spawn(move || {
            let result = rfd::FileDialog::new().pick_folder().map(|folder| {
                MusicQueue::from_folder(&folder)
                    .map_err(|err| format!("Cannot scan {}: {err}", folder.display()))
            }).transpose();
            let _ = sender.send(result);
        }) {
            Ok(_) => {
                self.pending = Some(PendingOpen::SelectingFolder(receiver));
                self.error = None;
            }
            Err(err) => self.error = Some(format!("Cannot start music folder picker: {err}")),
        }
    }

    fn start_load(&mut self, path: PathBuf) {
        self.start_queue_load(path.clone(), Some(MusicQueue::single(path)), 0, false);
    }

    fn start_queue_load(&mut self, path: PathBuf, queue: Option<MusicQueue>, index: usize, autoplay: bool) {
        if self.pending.is_some() {
            return;
        }
        let result = (|| {
            let device = cpal::default_host()
                .default_output_device()
                .ok_or_else(|| "No default audio output device is available".to_owned())?;
            let supported = device.default_output_config().map_err(|err| format!("Output config: {err}"))?;
            let format = match supported.sample_format() {
                SampleFormat::F32 => PcmSampleFormat::F32,
                SampleFormat::I16 => PcmSampleFormat::S16,
                SampleFormat::U16 => PcmSampleFormat::U16,
                other => return Err(format!("Unsupported default output sample format: {other:?}")),
            };
            let target = DeviceFormat {
                rate: supported.sample_rate(),
                channels: supported.channels(),
                sample_format: format,
            };
            let (sender, receiver) = mpsc::channel();
            let worker_path = path.clone();
            std::thread::Builder::new()
                .name("audio-file-decode".to_owned())
                .spawn(move || {
                    let _ = sender.send(prepare(&worker_path, target));
                })
                .map_err(|err| format!("Cannot start audio decoder: {err}"))?;
            Ok(PendingOpen::Loading { path, queue, index, autoplay, device, supported, receiver })
        })();
        match result {
            Ok(pending) => {
                self.pending = Some(pending);
                self.error = None;
            }
            Err(err) => self.error = Some(err),
        }
    }

    fn start_adjacent(&mut self, next: bool) {
        if self.pending.is_some() || self.track.is_none() {
            return;
        }
        let Some(queue) = &self.queue else { return };
        let index = if next { queue.next_index(self.repeat) } else { queue.previous_index(self.repeat) };
        if let Some(index) = index && Some(index) != queue.current_index() && let Some(path) = queue.path(index) {
            self.start_queue_load(path.to_path_buf(), None, index, true);
        }
    }

    fn update_completion(&mut self) {
        let Some(track) = &self.track else { return };
        if self.pending.is_some() || !track.ended.swap(false, Ordering::AcqRel) {
            return;
        }
        let Some(queue) = &self.queue else { return };
        let Some(index) = queue.completed_index(self.repeat) else { return };
        if Some(index) == queue.current_index() {
            // Repeat One reuses its already converted PCM and the same stream.
            if let Err(err) = track.play() {
                self.error = Some(err);
            }
        } else if let Some(path) = queue.path(index) {
            self.start_queue_load(path.to_path_buf(), None, index, true);
        }
    }

    fn update_open(&mut self) {
        enum Ready {
            Selected(Result<Option<PathBuf>, String>),
            Folder(Result<Result<Option<MusicQueue>, String>, String>),
            Prepared(Result<Result<Prepared, String>, String>),
        }
        let ready = match &self.pending {
            Some(PendingOpen::Selecting(receiver)) => poll_worker(receiver).map(Ready::Selected),
            Some(PendingOpen::SelectingFolder(receiver)) => poll_worker(receiver).map(Ready::Folder),
            Some(PendingOpen::Loading { receiver, .. }) => poll_worker(receiver).map(Ready::Prepared),
            None => None,
        };
        let Some(ready) = ready else { return };
        let pending = self.pending.take().expect("ready implies pending request");
        match (pending, ready) {
            (PendingOpen::Selecting(_), Ready::Selected(Ok(Some(path)))) => self.start_load(path),
            (PendingOpen::Selecting(_), Ready::Selected(Ok(None))) => {}
            (PendingOpen::Selecting(_), Ready::Selected(Err(err))) => self.error = Some(err),
            (PendingOpen::SelectingFolder(_), Ready::Folder(Ok(Ok(Some(queue))))) => {
                if let Some(path) = queue.path(0) {
                    self.start_queue_load(path.to_path_buf(), Some(queue), 0, false);
                } else {
                    self.error = Some("Music folder contains no supported audio files".to_owned());
                }
            }
            (PendingOpen::SelectingFolder(_), Ready::Folder(Ok(Ok(None)))) => {}
            (PendingOpen::SelectingFolder(_), Ready::Folder(Ok(Err(err)) | Err(err))) => self.error = Some(err),
            (PendingOpen::Loading { path, queue, index, autoplay, device, supported, .. }, Ready::Prepared(result)) => {
                let result = result.and_then(|prepared| prepared).and_then(|prepared| {
                    let volume = self.track.as_ref().map_or(1.0, |track| transport(&track.shared).volume());
                    let track = Track::from_prepared(&path, device, supported, prepared, volume)?;
                    if autoplay {
                        track.play()?;
                    }
                    Ok(track)
                });
                match result {
                    Ok(track) => {
                        if let Some(queue) = queue {
                            self.queue = Some(queue);
                        } else if let Some(queue) = &mut self.queue {
                            queue.set_current(index);
                        }
                        self.seek_preview = SeekPreview::default();
                        self.track = Some(track);
                        self.error = None;
                    }
                    Err(err) => self.error = Some(err),
                }
            }
            _ => unreachable!("worker result always matches the request phase"),
        }
    }
}

impl eframe::App for AudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "linux")]
        if let Some(tray) = &mut self.tray {
            tray.update(ui.ctx());
        }
        self.update_open();
        if let Some(err) = self.track.as_ref().and_then(|track| track.errors.try_recv().ok()) {
            // The stream is invalid: drop it rather than presenting a Playing UI.
            self.track = None;
            self.seek_preview = SeekPreview::default();
            self.error = Some(err);
        }
        self.update_completion();
        let ctx = ui.ctx().clone();
        let ctrl = egui::Modifiers::CTRL;
        let alt = egui::Modifiers::ALT;
        let plain = egui::Modifiers::NONE;
        // Reserve transport keys before any focused widget can interpret Space as
        // a click (e.g. the file picker) or Ctrl+arrows as a slider edit.
        let play_shortcut = shortcut(&ctx, plain, egui::Key::Space);
        let previous_shortcut = shortcut(&ctx, ctrl, egui::Key::ArrowLeft);
        let next_shortcut = shortcut(&ctx, ctrl, egui::Key::ArrowRight);
        let repeat_shortcut = shortcut(&ctx, plain, egui::Key::R);
        let mut open_file = self.pending.is_none() && shortcut(&ctx, ctrl, egui::Key::O);
        let mut open_folder = self.pending.is_none() && shortcut(&ctx, ctrl, egui::Key::F);
        let colour_page = shortcut(&ctx, alt, egui::Key::C);
        let music_page = shortcut(&ctx, alt, egui::Key::M);
        let back = self.sidebar_expanded && shortcut(&ctx, plain, egui::Key::Escape);
        let sidebar = shortcut(&ctx, ctrl, egui::Key::B);
        let colour_mode = [
            (egui::Key::B, ColorMode::BlackWhite),
            (egui::Key::S, ColorMode::Solid),
            (egui::Key::G, ColorMode::Rgb),
        ].into_iter().find_map(|(key, mode)| shortcut(&ctx, alt, key).then_some(mode));
        if let Some(mode) = colour_mode {
            self.color_mode = mode;
        }
        if colour_page || music_page {
            self.sidebar_expanded = true;
            self.sidebar_page = if colour_page { SidebarPage::Colour } else { SidebarPage::Music };
        }
        if back {
            if self.sidebar_page == SidebarPage::Menu {
                self.sidebar_expanded = false;
            }
            self.sidebar_page = SidebarPage::Menu;
        }
        if sidebar {
            self.sidebar_expanded = !self.sidebar_expanded;
            if !self.sidebar_expanded {
                self.sidebar_page = SidebarPage::Menu;
            }
        }
        if let Some(track) = &self.track {
            let up = shortcut(&ctx, plain, egui::Key::ArrowUp);
            let down = shortcut(&ctx, plain, egui::Key::ArrowDown);
            self.wheel_remainder += wheel_notches(&ctx);
            let wheel_steps = self.wheel_remainder.trunc();
            self.wheel_remainder -= wheel_steps;
            let steps = f32::from(u8::from(up)) - f32::from(u8::from(down)) + wheel_steps;
            if steps != 0.0 && let Some(volume) = change_volume(&track.shared, steps) {
                self.volume_status = Some((format!("Volume: {}%", (volume * 100.0).round() as u8), Instant::now()));
            }
        } else {
            self.wheel_remainder = 0.0;
        }
        if self.last_frame.is_none() {
            let mut visuals = egui::Visuals::dark();
            visuals.selection.bg_fill = egui::Color32::WHITE;
            visuals.selection.stroke.color = CANVAS;
            ui.ctx().set_visuals(visuals);
        }
        let now = Instant::now();
        if self.volume_status.as_ref().is_some_and(|(_, changed)| {
            self.track.is_none() || now.duration_since(*changed) >= Duration::from_secs(2)
        }) {
            self.volume_status = None;
        }
        let dt = self.last_frame.map_or(Duration::ZERO, |last| now.duration_since(last));
        self.last_frame = Some(now);

        if let Some(track) = &self.track {
            let (state, position) = {
                let player = transport(&track.shared);
                (player.state(), player.position())
            };
            self.donut.update(track.samples.as_slice(), track.channels, track.rate, position, state, dt);
        } else {
            self.donut.update(&[], 0, 0, 0, PlaybackState::Stopped, dt);
        }

        let target_width = if self.sidebar_expanded { 180.0 } else { 18.0 };
        let ease = 1.0 - (-12.0 * dt.as_secs_f32().min(0.1)).exp();
        self.sidebar_width += (target_width - self.sidebar_width) * ease;
        if (target_width - self.sidebar_width).abs() < 0.1 {
            self.sidebar_width = target_width;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(CANVAS))
            .show(ui, |ui| {
                let frame = ui.available_rect_before_wrap().shrink(12.0);
                let painter = ui.painter().clone();
                painter.rect_filled(frame, 22.0, FRAME);
                painter.rect_stroke(frame, 22.0, egui::Stroke::new(1.0, OUTLINE), egui::StrokeKind::Inside);

                let inner = frame.shrink(12.0);
                let divider = inner.left() + self.sidebar_width;
                painter.line_segment(
                    [egui::pos2(divider, inner.top()), egui::pos2(divider, inner.bottom())],
                    egui::Stroke::new(1.0, OUTLINE),
                );
                let handle = egui::Rect::from_min_size(
                    egui::pos2(inner.left() + (self.sidebar_width - 18.0) * 0.5, inner.top() + 12.0),
                    egui::vec2(18.0, 32.0),
                );
                if ui.put(
                    handle,
                    egui::Button::new(if self.sidebar_expanded { "‹" } else { "›" })
                        .fill(FRAME).stroke(egui::Stroke::NONE).frame(false),
                ).on_hover_text(if self.sidebar_expanded { "Collapse sidebar (Ctrl+B)" } else { "Expand sidebar (Ctrl+B)" }).clicked() {
                    self.sidebar_expanded = !self.sidebar_expanded;
                    if !self.sidebar_expanded {
                        self.sidebar_page = SidebarPage::Menu;
                    }
                }
                if self.sidebar_expanded && self.sidebar_width >= 176.0 {
                    let left = inner.left() + 12.0;
                    let row = |y, height| egui::Rect::from_min_size(
                        egui::pos2(left, inner.top() + y), egui::vec2(148.0, height),
                    );
                    match self.sidebar_page {
                        SidebarPage::Menu => {
                            if ui.put(row(66.0, 30.0), egui::Button::new("Colour").min_size(egui::vec2(148.0, 30.0)))
                                .on_hover_text("Colour settings (Alt+C)").clicked() {
                                self.sidebar_page = SidebarPage::Colour;
                            }
                            if ui.put(row(103.0, 30.0), egui::Button::new("Music").min_size(egui::vec2(148.0, 30.0)))
                                .on_hover_text("Music controls (Alt+M)").clicked() {
                                self.sidebar_page = SidebarPage::Music;
                            }
                        }
                        SidebarPage::Colour => {
                            if ui.put(row(66.0, 30.0), egui::Button::new("‹ Back").min_size(egui::vec2(148.0, 30.0)))
                                .on_hover_text("Back (Escape)").clicked() {
                                self.sidebar_page = SidebarPage::Menu;
                            }
                            for (index, (mode, label)) in [
                                (ColorMode::BlackWhite, "B&W"),
                                (ColorMode::Solid, "Solid"),
                                (ColorMode::Rgb, "RGB"),
                            ].into_iter().enumerate() {
                                let hotkey = match mode {
                                    ColorMode::BlackWhite => "Alt+B",
                                    ColorMode::Solid => "Alt+S",
                                    ColorMode::Rgb => "Alt+G",
                                };
                                if ui.put(
                                    row(109.0 + index as f32 * 33.0, 28.0),
                                    egui::RadioButton::new(self.color_mode == mode, label),
                                ).on_hover_text(hotkey).clicked() {
                                    self.color_mode = mode;
                                }
                            }
                            ui.put(row(217.0, 23.0), egui::Label::new("Tone (Solid/RGB)"));
                            ui.spacing_mut().slider_width = 136.0;
                            ui.add_enabled_ui(self.color_mode != ColorMode::BlackWhite, |ui| {
                                ui.put(
                                    row(243.0, 26.0),
                                    egui::Slider::new(&mut self.hue, 0.0..=1.0).show_value(false),
                                )
                            }).inner.on_hover_text("Shift the torus and contour hues");
                        }
                        SidebarPage::Music => {
                            if ui.put(row(66.0, 30.0), egui::Button::new("‹ Back").min_size(egui::vec2(148.0, 30.0)))
                                .on_hover_text("Back (Escape)").clicked() {
                                self.sidebar_page = SidebarPage::Menu;
                            }
                            if ui.add_enabled_ui(self.pending.is_none(), |ui| {
                                ui.put(row(109.0, 30.0), egui::Button::new("Open file").min_size(egui::vec2(148.0, 30.0)))
                            }).inner.on_hover_text("Open a local audio file (Ctrl+O)").clicked() {
                                open_file = true;
                            }
                            let folder = self.queue.as_ref().and_then(MusicQueue::folder);
                            let folder_label = folder.and_then(Path::file_name).unwrap_or_else(|| std::ffi::OsStr::new("Music folder"));
                            let response = ui.add_enabled_ui(self.pending.is_none(), |ui| {
                                ui.put(row(146.0, 30.0), egui::Button::new(folder_label.to_string_lossy())
                                    .truncate().min_size(egui::vec2(148.0, 30.0)))
                            }).inner;
                            let response = if let Some(path) = folder {
                                if response.hovered() {
                                    response.on_hover_text(format!("Music folder: {} (Ctrl+F to select another)", path.display()))
                                } else {
                                    response
                                }
                            } else {
                                response.on_hover_text("Select a folder and scan its audio files (Ctrl+F)")
                            };
                            if response.clicked() {
                                open_folder = true;
                            }
                        }
                    }
                }
                if open_file {
                    self.start_picker();
                } else if open_folder {
                    self.start_folder_picker();
                }

                let content = egui::Rect::from_min_max(
                    egui::pos2(divider + 14.0, inner.top()),
                    inner.right_bottom(),
                );
                let status_rect = egui::Rect::from_min_size(
                    egui::pos2(content.left() + 4.0, content.top() + 5.0),
                    egui::vec2((content.width() - 8.0).max(0.0), 28.0),
                );
                if let Some(error) = &self.error {
                    ui.put(status_rect, egui::Label::new(egui::RichText::new(error).color(egui::Color32::WHITE)).truncate())
                        .on_hover_text(error);
                } else {
                    match &self.pending {
                        Some(PendingOpen::Selecting(_)) => {
                            ui.put(status_rect, egui::Label::new("Selecting audio file…").truncate());
                        }
                        Some(PendingOpen::SelectingFolder(_)) => {
                            ui.put(status_rect, egui::Label::new("Selecting and scanning music folder…").truncate());
                        }
                        Some(PendingOpen::Loading { path, .. }) => {
                            ui.put(status_rect, egui::Label::new(format!(
                                "Loading {}…", path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()
                            )).truncate());
                        }
                        None => {
                            if let Some((label, _)) = &self.volume_status {
                                ui.put(status_rect, egui::Label::new(label.as_str()))
                                    .on_hover_text("Volume: mouse wheel or ↑/↓ while Rap is focused");
                            } else {
                                ui.put(status_rect, egui::Label::new("Ready"))
                                    .on_hover_text("Volume: mouse wheel or ↑/↓ while Rap is focused");
                            }
                        }
                    }
                }

                let title_y = inner.bottom() - 142.0;
                let display = egui::Rect::from_min_max(
                    egui::pos2(content.left(), inner.top() + 46.0),
                    egui::pos2(content.right(), title_y - 22.0),
                );
                let side = display.width().min(display.height()).max(0.0);
                let radius = side * 0.38;
                let center = display.center();
                self.donut.draw_equalizer(&painter, center, radius, self.color_mode, self.hue);
                let font = egui::FontId::monospace((11.0 * radius / 120.0).max(6.0));
                let galley = painter.layout_job(self.donut.colored_layout(font, self.color_mode, self.hue));
                let top_left = center - galley.size() * 0.5;
                painter.galley(top_left, galley, egui::Color32::WHITE);

                let title = self.track.as_ref().map_or("No audio file open", |track| track.title.as_str());
                let artist = self.track.as_ref()
                    .map_or("Open a local file to begin.", |track| track.artist.as_deref().unwrap_or("Исполнитель неизвестен"));
                let title_rect = egui::Rect::from_center_size(egui::pos2(content.center().x, title_y), egui::vec2(content.width(), 30.0));
                let artist_rect = title_rect.translate(egui::vec2(0.0, 29.0));
                ui.put(title_rect, egui::Label::new(egui::RichText::new(title).size(22.0).color(egui::Color32::WHITE))
                    .truncate().halign(egui::Align::Center));
                ui.put(artist_rect, egui::Label::new(egui::RichText::new(artist).size(15.0).color(MUTED))
                    .truncate().halign(egui::Align::Center));

                let button_width = 42.0;
                let gap = 12.0;
                let left = content.center().x - (4.0 * button_width + 3.0 * gap) * 0.5;
                let player_state = self.track.as_ref()
                    .map(|track| transport(&track.shared).state());
                let previous = self.queue.as_ref()
                    .is_some_and(|queue| queue.previous_index(self.repeat).is_some_and(|index| Some(index) != queue.current_index()));
                let next = self.queue.as_ref()
                    .is_some_and(|queue| queue.next_index(self.repeat).is_some_and(|index| Some(index) != queue.current_index()));
                for (index, icon, hover, enabled, hotkey) in [
                    (0, "⏮", "Previous track (Ctrl+←)", previous, previous_shortcut),
                    (1, if player_state == Some(PlaybackState::Playing) { "||" } else { "▶" },
                        if player_state == Some(PlaybackState::Playing) { "Pause (Space)" } else { "Play (Space)" },
                        self.track.is_some(), play_shortcut),
                    (2, "⏭", "Next track (Ctrl+→)", next, next_shortcut),
                ] {
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(left + index as f32 * (button_width + gap), title_y + 53.0),
                        egui::vec2(button_width, 32.0),
                    );
                    let response = ui.add_enabled_ui(enabled && self.pending.is_none(), |ui| {
                        ui.put(rect, egui::Button::new(icon).fill(FRAME)
                            .stroke(egui::Stroke::new(1.0, OUTLINE)).corner_radius(6.0))
                    }).inner.on_hover_text(hover);
                    if response.clicked() || enabled && self.pending.is_none() && hotkey {
                        match index {
                            0 => self.start_adjacent(false),
                            2 => self.start_adjacent(true),
                            _ => if let Some(track) = &self.track {
                                if player_state == Some(PlaybackState::Playing) {
                                    transport(&track.shared).pause();
                                } else if let Err(err) = track.play() {
                                    self.error = Some(err);
                                } else {
                                    self.error = None;
                                }
                            },
                        }
                    }
                }
                let repeat_rect = egui::Rect::from_min_size(
                    egui::pos2(left + 3.0 * (button_width + gap), title_y + 53.0),
                    egui::vec2(button_width, 32.0),
                );
                let (repeat_label, repeat_hover) = match self.repeat {
                    RepeatMode::Off => ("↻", "Repeat off (R: all)"),
                    RepeatMode::All => ("↻", "Repeat all (R: one)"),
                    RepeatMode::One => ("↻₁", "Repeat one (R: off)"),
                };
                let repeat_color = if self.repeat == RepeatMode::Off { MUTED } else { egui::Color32::WHITE };
                if ui.add_enabled_ui(self.track.is_some(), |ui| {
                    ui.put(repeat_rect, egui::Button::new(egui::RichText::new(repeat_label).color(repeat_color))
                        .fill(FRAME)
                        .stroke(egui::Stroke::new(1.0, if self.repeat == RepeatMode::Off { OUTLINE } else { egui::Color32::WHITE }))
                        .corner_radius(6.0))
                }).inner.on_hover_text(repeat_hover).clicked()
                    || self.track.is_some() && repeat_shortcut {
                    self.repeat = self.repeat.cycle();
                }

                let (position, total, rate) = self.track.as_ref().map_or((0, 0, 1), |track| {
                    let player = transport(&track.shared);
                    (player.position(), player.total_frames(), track.rate)
                });
                let mut seek = self.seek_preview.displayed(position);
                let seek_width = (content.width() - 110.0).clamp(80.0, 420.0);
                let seek_left = content.center().x - (seek_width + 98.0) * 0.5;
                let seek_y = title_y + 103.0;
                let current_rect = egui::Rect::from_min_size(egui::pos2(seek_left, seek_y), egui::vec2(46.0, 26.0));
                let seek_rect = egui::Rect::from_min_size(egui::pos2(seek_left + 48.0, seek_y), egui::vec2(seek_width, 26.0));
                let end_rect = egui::Rect::from_min_size(egui::pos2(seek_rect.right() + 4.0, seek_y), egui::vec2(46.0, 26.0));
                ui.put(current_rect, egui::Label::new(egui::RichText::new(format_time(seek, rate)).color(MUTED)));
                ui.spacing_mut().slider_width = seek_width;
                ui.spacing_mut().slider_rail_height = 2.0;
                let response = ui.add_enabled_ui(self.track.is_some(), |ui| {
                    ui.put(seek_rect, egui::Slider::new(&mut seek, 0..=total).show_value(false))
                }).inner;
                if let Some(target) = self.seek_preview.update(
                    seek,
                    response.changed(),
                    response.is_pointer_button_down_on(),
                    response.drag_stopped() || response.clicked(),
                ) && let Some(track) = &self.track {
                    seek_track(&track.shared, &track.ended, target);
                }
                ui.put(end_rect, egui::Label::new(egui::RichText::new(format_time(total, rate)).color(MUTED)));
            });
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
}

fn format_time(frames: u64, rate: u32) -> String {
    let secs = frames / u64::from(rate.max(1));
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::{AudioApp, GainRamp, PendingOpen, SeekPreview, change_volume, seek_track, shortcut, wheel_notches, render as render_output, transport};
    use crate::core::player::{PlaybackState, Player};
    use crate::core::queue::{MusicQueue, RepeatMode};
    use eframe::egui;
    use std::path::PathBuf;
    use std::sync::{Mutex, atomic::{AtomicBool, Ordering}, mpsc};

    #[test]
    fn shortcuts_require_viewport_focus_and_exact_modifiers() {
        let ctx = egui::Context::default();
        let key = |modifiers| egui::Event::Key {
            key: egui::Key::O, physical_key: None, pressed: true, repeat: false, modifiers,
        };
        for (focused, modifiers, accepted) in [
            (true, egui::Modifiers { command: true, ..egui::Modifiers::CTRL }, true),
            (false, egui::Modifiers { command: true, ..egui::Modifiers::CTRL }, false),
            (true, egui::Modifiers::CTRL | egui::Modifiers::ALT, false),
            (true, egui::Modifiers::NONE, false),
        ] {
            let raw = egui::RawInput { focused, events: vec![key(modifiers)], ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| {
                assert_eq!(shortcut(ui.ctx(), egui::Modifiers::CTRL, egui::Key::O), accepted);
                assert!(!shortcut(ui.ctx(), egui::Modifiers::CTRL, egui::Key::O));
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn shortcuts_use_physical_key_on_non_latin_layout_without_stealing_input() {
        let ctx = egui::Context::default();
        for (focused, modifiers, accepted) in [
            (true, egui::Modifiers::ALT, true),
            (false, egui::Modifiers::ALT, false),
            (true, egui::Modifiers::CTRL | egui::Modifiers::ALT, false),
        ] {
            let event = egui::Event::Key {
                key: egui::Key::F, physical_key: Some(egui::Key::M),
                pressed: true, repeat: false, modifiers,
            };
            let raw = egui::RawInput { focused, events: vec![event], ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| {
                assert_eq!(shortcut(ui.ctx(), egui::Modifiers::ALT, egui::Key::M), accepted);
                assert!(!shortcut(ui.ctx(), egui::Modifiers::ALT, egui::Key::M));
                assert_eq!(ui.input(|input| input.events.iter().any(|event| matches!(
                    event, egui::Event::Key { key: egui::Key::F, physical_key: Some(egui::Key::M), .. }
                ))), !accepted);
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn space_transport_does_not_activate_tab_focused_file_picker() {
        let ctx = egui::Context::default();
        let player = Mutex::new(Player::new(8));
        transport(&player).play();
        let tab = egui::Event::Key {
            key: egui::Key::Tab, physical_key: Some(egui::Key::Tab),
            pressed: true, repeat: false, modifiers: egui::Modifiers::NONE,
        };
        let mut first = ctx.run_ui(egui::RawInput { events: vec![tab], ..Default::default() }, |ui| {
            let _ = ui.button("Open file");
        });
        first.textures_delta.clear();

        let space = egui::Event::Key {
            key: egui::Key::Space, physical_key: Some(egui::Key::Space),
            pressed: true, repeat: false, modifiers: egui::Modifiers::NONE,
        };
        let mut second = ctx.run_ui(egui::RawInput { events: vec![space], ..Default::default() }, |ui| {
            let play_shortcut = shortcut(ui.ctx(), egui::Modifiers::NONE, egui::Key::Space);
            let picker = ui.button("Open file");
            assert!(picker.has_focus());
            assert!(!picker.clicked(), "Space must not open the picker");
            assert!(play_shortcut);
            transport(&player).pause();
        });
        second.textures_delta.clear();
        assert_eq!(transport(&player).state(), PlaybackState::Paused);
    }

    #[test]
    fn wheel_volume_changes_only_when_focused_and_stays_in_range() {
        let ctx = egui::Context::default();
        let player = Mutex::new(Player::new(10));
        transport(&player).set_volume(0.5);
        for (focused, point, expected) in [
            (true, egui::pos2(50.0, 50.0), 0.65),
            (false, egui::pos2(50.0, 50.0), 0.65),
            (true, egui::pos2(500.0, 500.0), 0.65),
        ] {
            let raw = egui::RawInput {
                focused,
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 200.0))),
                events: vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Line,
                        delta: egui::vec2(0.0, 3.0),
                        phase: egui::TouchPhase::Move,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            };
            let mut changed = None;
            let mut output = ctx.run_ui(raw, |ui| {
                changed = change_volume(&player, wheel_notches(ui.ctx()));
            });
            assert_eq!(changed.is_some(), focused && point.x < 200.0);
            if let Some(volume) = changed {
                assert!((volume - expected).abs() < 1e-6);
            }
            output.textures_delta.clear();
            assert!((transport(&player).volume() - expected).abs() < 1e-6);
        }
        assert_eq!(change_volume(&player, -30.0), Some(0.0));
        assert_eq!(change_volume(&player, -1.0), None);
        assert_eq!(transport(&player).volume(), 0.0);
        assert_eq!(change_volume(&player, 1.0), Some(0.05));
        assert!((transport(&player).volume() - 0.05).abs() < 1e-6);
    }

    #[test]
    fn seeking_away_from_eof_clears_completion_before_next_frame() {
        let player = Mutex::new(Player::new(2));
        let ended = AtomicBool::new(false);
        transport(&player).play();
        let mut gain = GainRamp::new(1.0, 400);
        let mut output = [0.0; 2];
        render_output(&mut output, 1, &[0.2, 0.4], &player, &ended, &mut gain);
        render_output(&mut output, 1, &[0.2, 0.4], &player, &ended, &mut gain);
        assert!(ended.load(Ordering::Acquire));
        seek_track(&player, &ended, 1);
        assert!(!ended.load(Ordering::Acquire));
        assert_eq!(transport(&player).position(), 1);
    }

    #[test]
    fn seek_preview_survives_playback_progress_and_commits_once_on_release() {
        let mut preview = SeekPreview::default();
        let mut player = Player::new(100);
        player.play();
        player.advance(10);
        assert_eq!(preview.update(10, false, true, false), None);
        player.advance(5);
        assert_eq!(preview.displayed(player.position()), 10);
        assert_eq!(preview.update(60, true, true, false), None);
        player.advance(5);
        assert_eq!(preview.displayed(player.position()), 60);
        assert_eq!(preview.update(60, false, true, false), None);
        assert_eq!(player.position(), 20);
        assert_eq!(preview.update(75, true, true, false), None);
        player.advance(5);
        assert_eq!(preview.displayed(player.position()), 75);
        assert_eq!(player.position(), 25);

        let mut seek_count = 0;
        if let Some(target) = preview.update(75, false, false, true) {
            player.seek(target);
            seek_count += 1;
        }
        assert_eq!(seek_count, 1);
        assert_eq!(player.position(), 75);
        assert_eq!(player.state(), PlaybackState::Playing);
        player.advance(4);
        assert_eq!(preview.displayed(player.position()), 79);
        assert_eq!(preview.update(79, false, false, false), None);
        assert_eq!(seek_count, 1);
    }

    #[test]
    fn seek_click_and_keyboard_commit_and_keep_paused_or_stopped_state() {
        let mut preview = SeekPreview::default();
        let mut player = Player::new(100);
        assert_eq!(preview.update(20, true, true, false), None); // Click presses and holds.
        assert_eq!(preview.update(20, false, false, true), Some(20)); // Release.
        player.seek(20);
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert_eq!(player.position(), 20);
        player.play();
        player.pause();
        let target = preview.update(30, true, false, false).expect("keyboard edit commits");
        player.seek(target);
        assert_eq!(player.state(), PlaybackState::Paused);
        assert_eq!(player.position(), 30);
        assert_eq!(preview.displayed(player.position()), 30);

        assert_eq!(preview.update(40, true, true, false), None);
        assert_eq!(preview.update(45, true, false, true), Some(45)); // Final release edit wins.
    }

    #[test]
    fn file_selection_cancellation_and_worker_failure_finish_without_device() {
        let mut app = AudioApp::new(None);
        let (sender, receiver) = mpsc::channel();
        app.pending = Some(PendingOpen::Selecting(receiver));
        app.start_picker();
        app.start_load(PathBuf::from("ignored.mp3"));
        assert!(matches!(app.pending, Some(PendingOpen::Selecting(_))));

        sender.send(None).unwrap();
        app.update_open();
        assert!(app.pending.is_none());
        assert!(app.error.is_none());

        let (sender, receiver) = mpsc::channel::<Option<PathBuf>>();
        app.pending = Some(PendingOpen::Selecting(receiver));
        drop(sender); // A picker panic closes the channel instead of hanging in Selecting.
        app.update_open();
        assert!(app.pending.is_none());
        assert_eq!(app.error.as_deref(), Some("File open worker stopped unexpectedly"));
    }

    #[test]
    fn folder_cancel_and_failure_preserve_queue_without_audio_device() {
        let mut app = AudioApp::new(None);
        app.queue = Some(MusicQueue::single(PathBuf::from("original.mp3")));
        app.repeat = RepeatMode::All;
        let (sender, receiver) = mpsc::channel();
        app.pending = Some(PendingOpen::SelectingFolder(receiver));
        sender.send(Ok(None)).unwrap();
        app.update_open();
        assert_eq!(app.queue.as_ref().and_then(MusicQueue::current), Some(std::path::Path::new("original.mp3")));
        assert_eq!(app.repeat, RepeatMode::All);

        let (sender, receiver) = mpsc::channel();
        app.pending = Some(PendingOpen::SelectingFolder(receiver));
        sender.send(Err("scan failed".to_owned())).unwrap();
        app.update_open();
        assert_eq!(app.queue.as_ref().and_then(MusicQueue::current), Some(std::path::Path::new("original.mp3")));
        assert_eq!(app.error.as_deref(), Some("scan failed"));
    }

    fn render<T: cpal::SizedSample + cpal::FromSample<f32>>(
        output: &mut [T], channels: usize, samples: &[f32], player: &Mutex<Player>, gain: &mut GainRamp,
    ) {
        render_output(output, channels, samples, player, &AtomicBool::new(false), gain);
    }

    #[test]
    fn only_natural_completion_signals_and_replaying_does_not_reuse_old_completion() {
        let player = Mutex::new(Player::new(1));
        let ended = AtomicBool::new(false);
        let mut gain = GainRamp::new(1.0, 400);
        let mut output = [0.0; 1];
        transport(&player).play();
        transport(&player).pause();
        render_output(&mut output, 1, &[0.75], &player, &ended, &mut gain);
        assert!(!ended.load(Ordering::Acquire));
        transport(&player).stop();
        render_output(&mut output, 1, &[0.75], &player, &ended, &mut gain);
        assert!(!ended.load(Ordering::Acquire));
        transport(&player).play();
        render_output(&mut output, 1, &[0.75], &player, &ended, &mut gain);
        assert_eq!(output, [0.75]);
        assert!(!ended.load(Ordering::Acquire));
        render_output(&mut output, 1, &[0.75], &player, &ended, &mut gain);
        assert!(ended.swap(false, Ordering::AcqRel));
        assert_eq!(transport(&player).state(), PlaybackState::Stopped);
        transport(&player).play();
        render_output(&mut output, 1, &[0.75], &player, &ended, &mut gain);
        assert!(!ended.load(Ordering::Acquire));
        let replacement_ended = AtomicBool::new(false);
        transport(&player).stop();
        transport(&player).play();
        render_output(&mut output, 1, &[0.75], &player, &replacement_ended, &mut gain);
        assert!(!replacement_ended.load(Ordering::Acquire));
    }

    #[test]
    fn xrun_does_not_drop_a_stream_that_cpal_recovers() {
        use cpal::{Error, ErrorKind};
        assert_eq!(super::stream_error_message(&Error::new(ErrorKind::Xrun)), None);
        assert!(super::stream_error_message(&Error::new(ErrorKind::DeviceNotAvailable))
            .as_deref()
            .is_some_and(|message| message.contains("Output stream failed")));
    }

    #[test]
    fn callback_silences_non_finite_pcm_before_device_conversion() {
        let player = Mutex::new(Player::new(6));
        let samples = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.5, 3.0, -3.0];
        let mut output = [f32::NAN; 6];
        let mut gain = GainRamp::new(0.5, 200);

        transport(&player).set_volume(0.5);
        transport(&player).play();
        render::<f32>(&mut output, 1, &samples, &player, &mut gain);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert_eq!(output, [0.0, 0.0, 0.0, 0.25, 1.0, -1.0]);

        transport(&player).seek(0);
        transport(&player).set_volume(0.0);
        render::<f32>(&mut output, 1, &samples, &player, &mut gain);
        assert_eq!(output, [0.0; 6]);
    }

    #[test]
    fn callback_respects_transport_seek_volume_and_eof() {
        let player = Mutex::new(Player::new(3));
        let samples = [0.5, -0.5, 1.0, -1.0, 0.25, -0.25];
        let mut output = [1.0_f32; 4];
        let mut gain = GainRamp::new(1.0, 400);
        render(&mut output, 2, &samples, &player, &mut gain);
        assert_eq!(output, [0.0; 4]);

        transport(&player).play();
        render(&mut output, 2, &samples, &player, &mut gain);
        assert_eq!(output, [0.5, -0.5, 1.0, -1.0]);
        assert_eq!(transport(&player).position(), 2);

        transport(&player).pause();
        render(&mut output, 2, &samples, &player, &mut gain);
        assert_eq!(output, [0.0; 4]);
        assert_eq!(transport(&player).position(), 2);

        transport(&player).set_volume(0.5);
        transport(&player).play();
        render(&mut output, 2, &samples, &player, &mut gain);
        assert_eq!(output, [0.1875, -0.1875, 0.0, 0.0]);
        assert_eq!(transport(&player).position(), 3);
        render(&mut output, 2, &samples, &player, &mut gain);
        assert_eq!(output, [0.0; 4]);
        assert_eq!(transport(&player).state(), PlaybackState::Stopped);
        assert_eq!(transport(&player).position(), 0);

        transport(&player).seek(1);
        transport(&player).play();
        render(&mut output, 2, &samples, &player, &mut gain);
        assert_eq!(output, [0.5, -0.5, 0.125, -0.125]);
        transport(&player).stop();
        assert_eq!(transport(&player).position(), 0);
    }

    #[test]
    fn first_play_after_endpoint_seek_emits_first_pcm_frame() {
        let player = Mutex::new(Player::new(3));
        let samples = [0.75, -0.5, 0.25];
        let mut output = [0.0; 2];
        let mut gain = GainRamp::new(1.0, 400);
        for paused in [false, true] {
            transport(&player).stop();
            if paused {
                transport(&player).play();
                transport(&player).pause();
            }
            transport(&player).seek(3);
            assert_eq!(transport(&player).position(), 3);
            transport(&player).play();
            render(&mut output, 1, &samples, &player, &mut gain);
            assert_eq!(output, [0.75, -0.5]);
            assert_eq!(transport(&player).position(), 2);
        }
    }

    #[test]
    fn callback_ramps_between_buffers_and_retargets_per_stereo_frame() {
        let player = Mutex::new(Player::new(20));
        let samples = [1.0, -1.0].repeat(20);
        let mut output = [1.0_f32; 4];
        let mut gain = GainRamp::new(0.0, 1000); // Five output frames per ramp.
        transport(&player).set_volume(0.0);
        transport(&player).play();
        render(&mut output, 2, &samples, &player, &mut gain);
        assert_eq!(output, [0.0; 4]);

        transport(&player).set_volume(1.0);
        render(&mut output, 2, &samples, &player, &mut gain);
        for (frame, expected) in output.as_chunks::<2>().0.iter().zip([0.2, 0.4]) {
            assert!((frame[0] - expected).abs() < 1e-6);
            assert!((frame[1] + expected).abs() < 1e-6);
        }

        // Retarget while still rising: start from 0.4, not from zero or one.
        transport(&player).set_volume(0.0);
        for expected in [[0.32, 0.24], [0.16, 0.08], [0.0, 0.0]] {
            render(&mut output, 2, &samples, &player, &mut gain);
            for (frame, expected) in output.as_chunks::<2>().0.iter().zip(expected) {
                assert!((frame[0] - expected).abs() < 1e-6);
                assert!((frame[1] + expected).abs() < 1e-6);
            }
        }
        assert_eq!(output, [0.0; 4]);

        transport(&player).set_volume(1.0);
        for expected in [[0.2, 0.4], [0.6, 0.8], [1.0, 1.0]] {
            render(&mut output, 2, &samples, &player, &mut gain);
            for (frame, expected) in output.as_chunks::<2>().0.iter().zip(expected) {
                assert!((frame[0] - expected).abs() < 1e-6);
                assert!((frame[1] + expected).abs() < 1e-6);
            }
        }
        assert_eq!(output, [1.0, -1.0, 1.0, -1.0]);
    }
}
