use std::time::{Duration, Instant};

use anyhow::Result;
use egui::{Color32, ComboBox};

use crate::audio::{self, AudioDevice, AudioPlayback};
use crate::platform;
use crate::types::{DeviceInfo, VideoFormat, VideoFrame};

pub struct App {
    video_devices: Vec<DeviceInfo>,
    audio_devices: Vec<AudioDevice>,
    selected_video: Option<usize>,
    selected_audio: Option<usize>,
    video_capture: Option<platform::VideoCapture>,
    audio_playback: Option<AudioPlayback>,
    last_error: Option<String>,
    mouse_y: f32,
    last_refresh: Instant,
    show_stats: bool,
    stats: StatsState,
    target_capture_size: Option<(u32, u32)>,
    disable_aspect_correction: bool,
    fullscreen: bool,
    fullscreen_request: Option<bool>,
}

struct StatsState {
    last_at: Instant,
    last_frames: u64,
    last_drops: u64,
    fps: f32,
    drops_per_s: f32,
    decode_us: u64,
    last_frame_format: Option<VideoFormat>,
}

impl StatsState {
    fn new() -> Self {
        Self {
            last_at: Instant::now(),
            last_frames: 0,
            last_drops: 0,
            fps: 0.0,
            drops_per_s: 0.0,
            decode_us: 0,
            last_frame_format: None,
        }
    }

    fn reset(&mut self) {
        self.last_at = Instant::now();
        self.last_frames = 0;
        self.last_drops = 0;
        self.fps = 0.0;
        self.drops_per_s = 0.0;
        self.decode_us = 0;
        self.last_frame_format = None;
    }

    fn update_frame(&mut self, frame: &VideoFrame) {
        self.last_frame_format = Some(frame.format);
    }
}

impl App {
    pub fn new() -> Result<Self> {
        let mut last_error = None;
        let video_devices = match platform::list_video_devices() {
            Ok(v) => v,
            Err(e) => {
                last_error = Some(format!("Video: {e}"));
                Vec::new()
            }
        };
        let audio_devices = match audio::list_input_devices() {
            Ok(v) => v,
            Err(e) => {
                last_error = Some(format!("Audio: {e}"));
                Vec::new()
            }
        };
        Ok(Self {
            video_devices,
            audio_devices,
            selected_video: None,
            selected_audio: None,
            video_capture: None,
            audio_playback: None,
            last_error,
            mouse_y: 0.0,
            last_refresh: Instant::now(),
            show_stats: false,
            stats: StatsState::new(),
            target_capture_size: None,
            disable_aspect_correction: false,
            fullscreen: false,
            fullscreen_request: None,
        })
    }

    pub fn set_mouse_y(&mut self, y: f32) {
        self.mouse_y = y;
    }

    pub fn set_target_capture_size(&mut self, size: Option<(u32, u32)>) {
        self.target_capture_size = size;
    }

    pub fn aspect_correction_enabled(&self) -> bool {
        !self.disable_aspect_correction
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    pub fn take_fullscreen_request(&mut self) -> Option<bool> {
        self.fullscreen_request.take()
    }

    pub fn set_fullscreen_state(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
    }

    pub fn capture_size(&self) -> Option<(u32, u32)> {
        self.video_capture
            .as_ref()
            .map(|cap| (cap.info.width, cap.info.height))
    }

    pub fn take_latest_frame(&mut self) -> Option<VideoFrame> {
        let cap = self.video_capture.as_ref()?;
        let mut latest = None;
        while let Ok(frame) = cap.rx.try_recv() {
            latest = Some(frame);
        }
        if self.show_stats {
            if let Some(frame) = latest.as_ref() {
                self.stats.update_frame(frame);
            }
        }
        latest
    }

    pub fn ui(&mut self, ctx: &egui::Context) {
        let ui_active =
            egui::Popup::is_any_open(ctx) || ctx.is_pointer_over_area() || ctx.is_using_pointer();
        let show_ui = self.video_capture.is_none() || self.mouse_y <= 32.0 || ui_active;
        if show_ui {
            egui::TopBottomPanel::top("selectors").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let mut vid = self.selected_video;
                    let video_text = vid
                        .and_then(|i| self.video_devices.get(i).map(|d| d.name.clone()))
                        .unwrap_or_else(|| "Video: None".to_string());
                    ComboBox::from_id_salt("video_select")
                        .selected_text(video_text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut vid, None, "None");
                            for (i, dev) in self.video_devices.iter().enumerate() {
                                ui.selectable_value(&mut vid, Some(i), &dev.name);
                            }
                        });
                    if vid != self.selected_video {
                        self.set_video(vid);
                    }
                    let mut aud = self.selected_audio;
                    let audio_text = aud
                        .and_then(|i| self.audio_devices.get(i).map(|d| d.info.name.clone()))
                        .unwrap_or_else(|| "Audio: None".to_string());
                    ComboBox::from_id_salt("audio_select")
                        .selected_text(audio_text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut aud, None, "None");
                            for (i, dev) in self.audio_devices.iter().enumerate() {
                                ui.selectable_value(&mut aud, Some(i), &dev.info.name);
                            }
                        });
                    if aud != self.selected_audio {
                        self.set_audio_with_reinit(aud);
                    }
                    let mut show_stats = self.show_stats;
                    if ui.checkbox(&mut show_stats, "Stats").changed() {
                        self.show_stats = show_stats;
                        self.apply_stats_enabled();
                    }
                    let mut disable_aspect = self.disable_aspect_correction;
                    if ui
                        .checkbox(&mut disable_aspect, "Disable Aspect-correct Rendering")
                        .changed()
                    {
                        self.disable_aspect_correction = disable_aspect;
                    }
                    let button_text = if self.fullscreen {
                        "Exit Fullscreen"
                    } else {
                        "Fullscreen"
                    };
                    if ui.button(button_text).clicked() {
                        let next = !self.fullscreen;
                        self.fullscreen_request = Some(next);
                    }
                });
                if let Some(err) = &self.last_error {
                    ui.colored_label(Color32::LIGHT_RED, err);
                }
            });
        }
        if self.show_stats {
            self.update_stats();
            if let Some(cap) = self.video_capture.as_ref() {
                let queue_len = cap.rx.len();
                let info = &cap.info;
                let fps_text = info
                    .fps
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "auto".to_string());
                egui::Area::new("stats_overlay".into())
                    .fixed_pos(egui::pos2(8.0, 40.0))
                    .show(ctx, |ui| {
                        ui.label(format!(
                            "Video: {} {}x{} @{}",
                            info.format, info.width, info.height, fps_text
                        ));
                        ui.label(format!("FPS: {:.1}", self.stats.fps));
                        ui.label(format!("Decode: {} us", self.stats.decode_us));
                        ui.label(format!("Drops/s: {:.1}", self.stats.drops_per_s));
                        ui.label(format!("Queue: {queue_len}"));
                        if let Some(fmt) = self.stats.last_frame_format {
                            ui.label(format!("Frame: {}", Self::format_name(fmt)));
                        }
                    });
            }
        }
        if show_ui && self.last_refresh.elapsed().as_secs() >= 5 {
            self.refresh_devices();
        }
    }

    pub fn toggle_stats(&mut self) {
        self.show_stats = !self.show_stats;
        self.apply_stats_enabled();
    }

    fn refresh_devices(&mut self) {
        self.last_refresh = Instant::now();
        if let Ok(v) = platform::list_video_devices() {
            self.video_devices = v;
            if let Some(idx) = self.selected_video {
                if idx >= self.video_devices.len() {
                    self.set_video(None);
                }
            }
        }
        if self.audio_devices.is_empty() {
            if let Ok(v) = audio::list_input_devices() {
                self.audio_devices = v;
                if let Some(idx) = self.selected_audio {
                    if idx >= self.audio_devices.len() {
                        self.set_audio(None);
                    }
                }
            }
        }
    }

    fn set_video(&mut self, sel: Option<usize>) {
        if let Some(mut cap) = self.video_capture.take() {
            cap.stop();
        }
        self.selected_video = sel;
        if let Some(i) = sel {
            match platform::start_video_capture(
                &self.video_devices[i].id,
                self.target_capture_size,
            ) {
                Ok(cap) => {
                    self.video_capture = Some(cap);
                    self.apply_stats_enabled();
                    self.last_error = None;
                }
                Err(e) => self.last_error = Some(format!("Video: {e}")),
            }
        }
    }

    fn apply_stats_enabled(&mut self) {
        if let Some(cap) = self.video_capture.as_ref() {
            cap.stats.set_enabled(self.show_stats);
            if self.show_stats {
                cap.stats.reset();
                self.stats.reset();
            }
        }
    }

    fn update_stats(&mut self) {
        let cap = match self.video_capture.as_ref() {
            Some(c) => c,
            None => return,
        };
        let snap = cap.stats.snapshot();
        let now = Instant::now();
        let dt = now.duration_since(self.stats.last_at);
        if dt >= Duration::from_millis(250) {
            let df = snap.frames.saturating_sub(self.stats.last_frames);
            let dd = snap.drops.saturating_sub(self.stats.last_drops);
            let secs = dt.as_secs_f32().max(0.001);
            self.stats.fps = df as f32 / secs;
            self.stats.drops_per_s = dd as f32 / secs;
            self.stats.last_frames = snap.frames;
            self.stats.last_drops = snap.drops;
            self.stats.last_at = now;
        }
        self.stats.decode_us = snap.decode_us;
    }

    fn format_name(format: VideoFormat) -> &'static str {
        match format {
            VideoFormat::Rgba => "RGBA",
            VideoFormat::Yuyv => "YUYV",
            VideoFormat::Nv12 => "NV12",
        }
    }

    fn set_audio(&mut self, sel: Option<usize>) {
        self.audio_playback = None;
        self.selected_audio = sel;
        if let Some(i) = sel {
            match audio::start_playback(&self.audio_devices[i]) {
                Ok(playback) => {
                    self.audio_playback = Some(playback);
                    self.last_error = None;
                }
                Err(e) => self.last_error = Some(format!("Audio: {e}")),
            }
        }
    }

    // This function exists because just setting the audio device doesn't work
    // But setting it again works
    // Basically the issue is "selecting the audio device plays the sound for a second and then nothing... you have to then select none and then back to the audio device for it to actually play the sound"
    fn set_audio_with_reinit(&mut self, sel: Option<usize>) {
        match sel {
            Some(index) => {
                self.set_audio(Some(index));
                self.set_audio(None);
                self.set_audio(Some(index));
                self.set_audio(None);
                self.set_audio(Some(index));
            }
            None => self.set_audio(None),
        }
    }
}
