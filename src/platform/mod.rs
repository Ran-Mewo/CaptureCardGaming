use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::sync::atomic::AtomicU64;
use std::thread::JoinHandle;

use anyhow::Result;
use crossbeam_channel::{bounded, Receiver};

use crate::types::{DeviceInfo, VideoFrame};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::KeepAwake;
#[cfg(target_os = "windows")]
pub use windows::KeepAwake;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub struct KeepAwake;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
impl KeepAwake {
    pub fn new() -> Option<Self> {
        None
    }
}

pub struct VideoCapture {
    pub rx: Receiver<VideoFrame>,
    pub info: VideoInfo,
    pub stats: Arc<CaptureStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VideoCapture {
    pub fn stop(&mut self) {
        if let Some(handle) = self.thread.take() {
            self.stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Debug)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub fps: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StatsSnapshot {
    pub frames: u64,
    pub drops: u64,
    pub decode_us: u64,
}

pub struct CaptureStats {
    enabled: AtomicBool,
    frames: AtomicU64,
    drops: AtomicU64,
    decode_us: AtomicU64,
}

impl CaptureStats {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            drops: AtomicU64::new(0),
            decode_us: AtomicU64::new(0),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        self.frames.store(0, Ordering::Relaxed);
        self.drops.store(0, Ordering::Relaxed);
        self.decode_us.store(0, Ordering::Relaxed);
    }

    pub fn on_frame_enabled(&self, decode_us: u64) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.decode_us.store(decode_us, Ordering::Relaxed);
    }

    pub fn on_drop_enabled(&self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            frames: self.frames.load(Ordering::Relaxed),
            drops: self.drops.load(Ordering::Relaxed),
            decode_us: self.decode_us.load(Ordering::Relaxed),
        }
    }
}

pub fn list_video_devices() -> Result<Vec<DeviceInfo>> {
    #[cfg(target_os = "linux")]
    {
        return linux::list_video_devices();
    }
    #[cfg(target_os = "windows")]
    {
        return windows::list_video_devices();
    }
    #[allow(unreachable_code)]
    Ok(Vec::new())
}

pub fn start_video_capture(id: &str, max_size: Option<(u32, u32)>) -> Result<VideoCapture> {
    let (tx, rx) = bounded(1);
    let drop_rx = rx.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(CaptureStats::new());
    #[cfg(target_os = "linux")]
    {
        let (thread, info) =
            linux::spawn_capture(id, max_size, tx, drop_rx, stop.clone(), stats.clone())?;
        return Ok(VideoCapture {
            rx,
            info,
            stats,
            stop,
            thread: Some(thread),
        });
    }
    #[cfg(target_os = "windows")]
    {
        let (thread, info) =
            windows::spawn_capture(id, max_size, tx, drop_rx, stop.clone(), stats.clone())?;
        return Ok(VideoCapture {
            rx,
            info,
            stats,
            stop,
            thread: Some(thread),
        });
    }
    #[allow(unreachable_code)]
    Ok(VideoCapture {
        rx,
        info: VideoInfo {
            width: 0,
            height: 0,
            format: "Unknown".to_string(),
            fps: None,
        },
        stats,
        stop,
        thread: None,
    })
}
