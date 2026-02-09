use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::collections::HashMap;
use std::io::Cursor;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{anyhow, Result};
use crossbeam_channel::Sender;
use crossbeam_channel::Receiver;
use gstreamer as gst;
use gstreamer::prelude::*;
use jpeg_decoder::{Decoder, PixelFormat};
use gstreamer_app::AppSink;
use gstreamer_video::{
    VideoColorMatrix as GstColorMatrix,
    VideoColorRange as GstColorRange,
    VideoFormat as GstVideoFormat,
    VideoInfo as GstVideoInfo,
};
use v4l::buffer::Type;
use v4l::capability::Flags;
use v4l::device::Device;
use v4l::format::FourCC;
use v4l::frameinterval::FrameIntervalEnum;
use v4l::io::mmap::Stream as MmapStream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;

use crate::types::{
    ColorInfo,
    ColorMatrix,
    ColorRange,
    DeviceInfo,
    FrameData,
    VideoFormat,
    VideoFrame,
};
use super::{CaptureStats, VideoInfo};

pub fn list_video_devices() -> Result<Vec<DeviceInfo>> {
    let mut raw = Vec::new();
    for node in v4l::context::enum_devices() {
        let path = node.path().display().to_string();
        let dev = match Device::with_path(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let caps = match dev.query_caps() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let flags = caps.capabilities;
        let capture = flags.contains(Flags::VIDEO_CAPTURE)
            || flags.contains(Flags::VIDEO_CAPTURE_MPLANE);
        if !capture || !flags.contains(Flags::STREAMING) {
            continue;
        }
        let name = node
            .name()
            .or(Some(caps.card))
            .unwrap_or_else(|| path.clone());
        raw.push((path, name));
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (_, name) in &raw {
        *counts.entry(name.clone()).or_default() += 1;
    }
    let out = raw
        .into_iter()
        .map(|(path, name)| {
            let display = if counts.get(&name).copied().unwrap_or(0) > 1 {
                let suffix = path.rsplit('/').next().unwrap_or(&path);
                format!("{name} ({suffix})")
            } else {
                name
            };
            DeviceInfo {
                id: path,
                name: display,
            }
        })
        .collect();
    Ok(out)
}

fn max_fps(dev: &Device, fourcc: FourCC, width: u32, height: u32) -> Option<f64> {
    let intervals = dev.enum_frameintervals(fourcc, width, height).ok()?;
    let mut best: Option<f64> = None;
    for interval in intervals {
        let frac = match interval.interval {
            FrameIntervalEnum::Discrete(f) => f,
            FrameIntervalEnum::Stepwise(s) => s.min,
        };
        if frac.numerator == 0 {
            continue;
        }
        let fps = frac.denominator as f64 / frac.numerator as f64;
        if best.map(|b| fps > b).unwrap_or(true) {
            best = Some(fps);
        }
    }
    best
}

#[derive(Clone, Copy)]
struct FormatChoice {
    fourcc: FourCC,
    width: u32,
    height: u32,
    fps: Option<f64>,
}

fn format_rank(fourcc: FourCC) -> u8 {
    if fourcc == FourCC::new(b"NV12") {
        3
    } else if fourcc == FourCC::new(b"YUYV") {
        2
    } else if fourcc == FourCC::new(b"MJPG") {
        1
    } else {
        0
    }
}

fn compare_choice(a: &FormatChoice, b: &FormatChoice) -> std::cmp::Ordering {
    let area_a = a.width * a.height;
    let area_b = b.width * b.height;
    match area_a.cmp(&area_b) {
        std::cmp::Ordering::Equal => {
            let fps_a = a.fps.unwrap_or(0.0);
            let fps_b = b.fps.unwrap_or(0.0);
            if (fps_a - fps_b).abs() > 0.1 {
                fps_a
                    .partial_cmp(&fps_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            } else {
                format_rank(a.fourcc).cmp(&format_rank(b.fourcc))
            }
        }
        other => other,
    }
}

fn best_choice_for_fourcc(dev: &Device, fourcc: FourCC) -> Option<FormatChoice> {
    let sizes = dev.enum_framesizes(fourcc).ok()?;
    let mut best: Option<FormatChoice> = None;
    for size in sizes {
        for d in size.size.to_discrete() {
            let fps = max_fps(dev, fourcc, d.width, d.height);
            let area = d.width * d.height;
            let better = match &best {
                None => true,
                Some(cur) => {
                    let cur_area = cur.width * cur.height;
                    if area != cur_area {
                        area > cur_area
                    } else {
                        let cur_fps = cur.fps.unwrap_or(0.0);
                        let new_fps = fps.unwrap_or(0.0);
                        new_fps > cur_fps
                    }
                }
            };
            if better {
                best = Some(FormatChoice {
                    fourcc,
                    width: d.width,
                    height: d.height,
                    fps,
                });
            }
        }
    }
    best
}

fn select_format(dev: &Device, max_size: Option<(u32, u32)>) -> Result<(v4l::Format, Option<u32>)> {
    let current = dev.format()?;
    let yuyv = FourCC::new(b"YUYV");
    let nv12 = FourCC::new(b"NV12");
    let mjpg = FourCC::new(b"MJPG");
    let supported = dev.enum_formats()?;
    let mut choices = Vec::new();
    for fourcc in [nv12, yuyv, mjpg] {
        if supported.iter().any(|f| f.fourcc == fourcc) {
            if let Some(choice) = best_choice_for_fourcc(dev, fourcc) {
                choices.push(choice);
            }
        }
    }
    if let Some(preferred) = choices
        .iter()
        .max_by_key(|c| c.width * c.height)
        .map(|c| c.width as f32 / c.height as f32)
    {
        let filtered: Vec<FormatChoice> = choices
            .iter()
            .copied()
            .filter(|c| {
                let ratio = c.width as f32 / c.height as f32;
                (ratio - preferred).abs() < 0.02
            })
            .collect();
        if !filtered.is_empty() {
            choices = filtered;
        }
    }
    if let Some((max_w, max_h)) = max_size {
        let filtered: Vec<FormatChoice> = choices
            .iter()
            .copied()
            .filter(|c| c.width <= max_w && c.height <= max_h)
            .collect();
        if !filtered.is_empty() {
            choices = filtered;
        }
    }
    choices.sort_by(|a, b| compare_choice(b, a));
    for choice in choices {
        if let Ok(set) =
            dev.set_format(&v4l::Format::new(choice.width, choice.height, choice.fourcc))
        {
            let fps = choice.fps.map(|v| v.round().max(1.0) as u32);
            return Ok((set, fps));
        }
    }
    if current.fourcc == yuyv || current.fourcc == nv12 || current.fourcc == mjpg {
        return Ok((current, None));
    }
    Err(anyhow!("Unsupported pixel format: {}", current.fourcc))
}

pub fn spawn_capture(
    id: &str,
    max_size: Option<(u32, u32)>,
    tx: Sender<VideoFrame>,
    drop_rx: Receiver<VideoFrame>,
    stop: Arc<AtomicBool>,
    stats: Arc<CaptureStats>,
) -> Result<(JoinHandle<()>, VideoInfo)> {
    let mut dev = Device::with_path(id)?;
    let (fmt, _fps) = select_format(&dev, max_size)?;
    if fmt.fourcc == FourCC::new(b"MJPG") {
        if let Some(decoder) = mjpeg_hw_decoder() {
            drop(dev);
            if let Ok((handle, info)) = spawn_capture_gst(
                id,
                fmt,
                decoder,
                tx.clone(),
                drop_rx.clone(),
                stop.clone(),
                stats.clone(),
            ) {
                return Ok((handle, info));
            }
            dev = Device::with_path(id)?;
            let _ = dev.set_format(&fmt);
        }
    }
    let width = fmt.width;
    let height = fmt.height;
    let fourcc = fmt.fourcc;
    let info = VideoInfo {
        width,
        height,
        format: format!("{fourcc}"),
        fps: None,
    };
    let stride = if fmt.stride == 0 {
        match fourcc {
            f if f == FourCC::new(b"YUYV") => width * 2,
            _ => width,
        }
    } else {
        fmt.stride
    } as usize;
    let handle = std::thread::Builder::new()
        .name("v4l-capture".to_string())
        .spawn(move || {
            let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 1) {
                Ok(s) => s,
                Err(_) => match MmapStream::with_buffers(&dev, Type::VideoCapture, 2) {
                    Ok(s) => s,
                    Err(_) => return,
                },
            };
            while !stop.load(Ordering::Relaxed) {
                let stats_on = stats.enabled();
                let (data, meta) = match stream.next() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let used = meta.bytesused as usize;
                let slice = &data[..used.min(data.len())];
                if !drop_rx.is_empty() {
                    if stats_on {
                        stats.on_drop_enabled();
                    }
                    continue;
                }
                let t0 = if stats_on { Some(Instant::now()) } else { None };
                let frame = if fourcc == FourCC::new(b"YUYV") {
                    VideoFrame {
                        width,
                        height,
                        format: VideoFormat::Yuyv,
                        stride,
                        uv_stride: 0,
                        color: ColorInfo::default_for_size(width),
                        data: FrameData::Owned(slice.to_vec()),
                    }
                } else if fourcc == FourCC::new(b"NV12") {
                    VideoFrame {
                        width,
                        height,
                        format: VideoFormat::Nv12,
                        stride,
                        uv_stride: stride,
                        color: ColorInfo::default_for_size(width),
                        data: FrameData::Owned(slice.to_vec()),
                    }
                } else if fourcc == FourCC::new(b"MJPG") {
                    match decode_mjpeg(slice) {
                        Ok((w, h, rgba)) => VideoFrame {
                            width: w,
                            height: h,
                            format: VideoFormat::Rgba,
                            stride: (w * 4) as usize,
                            uv_stride: 0,
                            color: ColorInfo::default_for_size(w),
                            data: FrameData::Owned(rgba),
                        },
                        Err(_) => continue,
                    }
                } else {
                    continue;
                };
                if let Some(t0) = t0 {
                    stats.on_frame_enabled(t0.elapsed().as_micros() as u64);
                }
                if let Err(err) = tx.try_send(frame) {
                    let frame = err.into_inner();
                    let _ = drop_rx.try_recv();
                    if stats_on {
                        stats.on_drop_enabled();
                    }
                    let _ = tx.try_send(frame);
                }
            }
        })?;
    Ok((handle, info))
}

fn rgb24_to_rgba(pixels: &[u8], pixel_count: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    // Safety: we set the length then write every byte.
    unsafe {
        rgba.set_len(pixel_count * 4);
        let mut src = pixels.as_ptr();
        let mut dst = rgba.as_mut_ptr();
        for _ in 0..pixel_count {
            *dst = *src;
            *dst.add(1) = *src.add(1);
            *dst.add(2) = *src.add(2);
            *dst.add(3) = 255;
            src = src.add(3);
            dst = dst.add(4);
        }
    }
    rgba
}

fn l8_to_rgba(pixels: &[u8], pixel_count: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    // Safety: we set the length then write every byte.
    unsafe {
        rgba.set_len(pixel_count * 4);
        let mut src = pixels.as_ptr();
        let mut dst = rgba.as_mut_ptr();
        for _ in 0..pixel_count {
            let v = *src;
            *dst = v;
            *dst.add(1) = v;
            *dst.add(2) = v;
            *dst.add(3) = 255;
            src = src.add(1);
            dst = dst.add(4);
        }
    }
    rgba
}

fn decode_mjpeg(data: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = Decoder::new(Cursor::new(data));
    let pixels = decoder.decode()?;
    let info = decoder.info().ok_or_else(|| anyhow!("Missing MJPEG info"))?;
    let width = info.width as u32;
    let height = info.height as u32;
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("MJPEG size overflow"))?;
    let rgba = match info.pixel_format {
        PixelFormat::RGB24 => {
            let expected = pixel_count
                .checked_mul(3)
                .ok_or_else(|| anyhow!("MJPEG size overflow"))?;
            if pixels.len() < expected {
                return Err(anyhow!("MJPEG RGB size mismatch"));
            }
            rgb24_to_rgba(&pixels[..expected], pixel_count)
        }
        PixelFormat::L8 => {
            if pixels.len() < pixel_count {
                return Err(anyhow!("MJPEG L8 size mismatch"));
            }
            l8_to_rgba(&pixels[..pixel_count], pixel_count)
        }
        _ => return Err(anyhow!("Unsupported MJPEG pixel format")),
    };
    Ok((width, height, rgba))
}

fn color_info_from_gst(info: &GstVideoInfo, source_fourcc: FourCC) -> ColorInfo {
    let colorimetry = info.colorimetry();
    let mut out = ColorInfo::default_for_size(info.width());
    if source_fourcc == FourCC::new(b"MJPG") {
        out.range = ColorRange::Full;
    }
    out.range = match colorimetry.range() {
        GstColorRange::Range0_255 => ColorRange::Full,
        GstColorRange::Range16_235 => ColorRange::Limited,
        _ => out.range,
    };
    out.matrix = match colorimetry.matrix() {
        GstColorMatrix::Bt709 => ColorMatrix::Bt709,
        GstColorMatrix::Bt2020 => ColorMatrix::Bt2020,
        GstColorMatrix::Bt601 | GstColorMatrix::Fcc | GstColorMatrix::Smpte240m => {
            ColorMatrix::Bt601
        }
        _ => out.matrix,
    };
    out
}

fn mjpeg_hw_decoder() -> Option<&'static str> {
    for name in [
        "nvjpegdec",
        "vaapijpegdec",
        "v4l2jpegdec",
        "qsvjpegdec",
    ] {
        if gst::ElementFactory::find(name).is_some() {
            return Some(name);
        }
    }
    None
}

fn mjpeg_pipeline_variants(
    device: &str,
    width: u32,
    height: u32,
    decoder: &str,
) -> Vec<String> {
    let base = format!("v4l2src device={device} io-mode=2 do-timestamp=true");
    let queue = "queue leaky=downstream max-size-buffers=1 max-size-time=0 max-size-bytes=0";
    let appsink =
        "appsink name=sink max-buffers=1 drop=true sync=false async=false enable-last-sample=false";
    let caps = format!("video/x-raw,format=NV12,width={width},height={height}");
    let jpegparse = if gst::ElementFactory::find("jpegparse").is_some() {
        "jpegparse ! "
    } else {
        ""
    };
    let mut variants = Vec::new();
    if decoder == "vaapijpegdec" && gst::ElementFactory::find("vaapipostproc").is_some() {
        variants.push(format!(
            "{base} ! image/jpeg ! {jpegparse}{queue} ! {decoder} ! vaapipostproc format=nv12 ! {caps} ! {appsink}"
        ));
    }
    variants.push(format!(
        "{base} ! image/jpeg ! {jpegparse}{queue} ! {decoder} ! {queue} ! {caps} ! {appsink}"
    ));
    variants.push(format!(
        "{base} ! image/jpeg ! {jpegparse}{queue} ! {decoder} ! {queue} ! videoconvert ! {caps} ! {appsink}"
    ));
    variants
}

fn launch_pipeline(pipeline_str: &str) -> Result<(gst::Pipeline, AppSink)> {
    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("GStreamer pipeline type"))?;
    let appsink = pipeline
        .by_name("sink")
        .ok_or_else(|| anyhow!("GStreamer appsink missing"))?
        .downcast::<AppSink>()
        .map_err(|_| anyhow!("GStreamer appsink type"))?;
    pipeline.set_state(gst::State::Playing)?;
    let (state_res, state, _) = pipeline.state(gst::ClockTime::from_mseconds(500));
    if state_res.is_err() || state != gst::State::Playing {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(anyhow!("GStreamer failed to play"));
    }
    Ok((pipeline, appsink))
}

fn build_mjpeg_pipeline(
    device: &str,
    width: u32,
    height: u32,
    decoder: &str,
) -> Result<(gst::Pipeline, AppSink)> {
    let mut last_err = None;
    for pipeline_str in mjpeg_pipeline_variants(device, width, height, decoder) {
        match launch_pipeline(&pipeline_str) {
            Ok(ok) => return Ok(ok),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("GStreamer failed to play")))
}

fn spawn_capture_gst(
    id: &str,
    fmt: v4l::Format,
    decoder: &str,
    tx: Sender<VideoFrame>,
    drop_rx: Receiver<VideoFrame>,
    stop: Arc<AtomicBool>,
    stats: Arc<CaptureStats>,
) -> Result<(JoinHandle<()>, VideoInfo)> {
    gst::init()?;
    let width = fmt.width;
    let height = fmt.height;
    let source_fourcc = fmt.fourcc;
    if source_fourcc != FourCC::new(b"MJPG") {
        return Err(anyhow!("GStreamer MJPG only"));
    }
    let (pipeline, appsink) = build_mjpeg_pipeline(id, width, height, decoder)?;
    let info = VideoInfo {
        width,
        height,
        format: format!("{}", fmt.fourcc),
        fps: None,
    };
    let handle = std::thread::Builder::new()
        .name("gst-capture".to_string())
        .spawn(move || {
            let mut gst_info: Option<GstVideoInfo> = None;
            let mut color_info: Option<ColorInfo> = None;
            while !stop.load(Ordering::Relaxed) {
                let stats_on = stats.enabled();
                let sample = match appsink.pull_sample() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if !drop_rx.is_empty() {
                    if stats_on {
                        stats.on_drop_enabled();
                    }
                    continue;
                }
                if gst_info.is_none() {
                    if let Some(caps) = sample.caps() {
                        gst_info = GstVideoInfo::from_caps(caps).ok();
                    }
                }
                let info = match &gst_info {
                    Some(i) => i,
                    None => continue,
                };
                let buffer = match sample.buffer().map(|b| b.to_owned()) {
                    Some(b) => b,
                    None => continue,
                };
                let color = match color_info {
                    Some(c) => c,
                    None => {
                        let c = color_info_from_gst(info, source_fourcc);
                        color_info = Some(c);
                        c
                    }
                };
                let t0 = if stats_on { Some(Instant::now()) } else { None };
                let (format, stride, uv_stride) = match info.format() {
                    GstVideoFormat::Nv12 => (
                        VideoFormat::Nv12,
                        info.stride()[0] as usize,
                        info.stride()[1] as usize,
                    ),
                    GstVideoFormat::Yuy2 => (
                        VideoFormat::Yuyv,
                        info.stride()[0] as usize,
                        0,
                    ),
                    GstVideoFormat::Rgba => (
                        VideoFormat::Rgba,
                        info.stride()[0] as usize,
                        0,
                    ),
                    _ => continue,
                };
                if let Some(t0) = t0 {
                    stats.on_frame_enabled(t0.elapsed().as_micros() as u64);
                }
                let frame = VideoFrame {
                    width: info.width(),
                    height: info.height(),
                    format,
                    stride,
                    uv_stride,
                    color,
                    data: FrameData::Gst(buffer),
                };
                if let Err(err) = tx.try_send(frame) {
                    let frame = err.into_inner();
                    let _ = drop_rx.try_recv();
                    if stats_on {
                        stats.on_drop_enabled();
                    }
                    let _ = tx.try_send(frame);
                }
            }
            let _ = pipeline.set_state(gst::State::Null);
        })?;
    Ok((handle, info))
}

pub struct KeepAwake {
    child: Child,
}

impl KeepAwake {
    pub fn new() -> Option<Self> {
        let child = Command::new("systemd-inhibit")
            .arg("--what=idle:sleep")
            .arg("--mode=block")
            .arg("--who=CaptureCardGaming")
            .arg("--why=CaptureCardGaming")
            .arg("sleep")
            .arg("infinity")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        Some(Self { child })
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
