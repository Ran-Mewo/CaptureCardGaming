use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;

use anyhow::{anyhow, Result};
use crossbeam_channel::{Receiver, Sender};
use windows::core::{GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};

use crate::pixel;
use crate::types::{ColorInfo, DeviceInfo, FrameData, VideoFormat, VideoFrame};
use super::{CaptureStats, VideoInfo};

pub fn list_video_devices() -> Result<Vec<DeviceInfo>> {
    let _com = ComInit::new()?;
    mf_startup()?;
    unsafe {
        let attrs = create_attrs()?;
        let mut activates = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut activates, &mut count)?;
        let slice = std::slice::from_raw_parts(activates, count as usize);
        let mut out = Vec::new();
        for act in slice.iter().flatten() {
            let name = get_string(act, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)?;
            let id = get_string(act, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)?;
            out.push(DeviceInfo { id, name });
        }
        CoTaskMemFree(Some(activates as _));
        Ok(out)
    }
}

pub fn spawn_capture(
    id: &str,
    _max_size: Option<(u32, u32)>,
    tx: Sender<VideoFrame>,
    drop_rx: Receiver<VideoFrame>,
    stop: Arc<AtomicBool>,
    stats: Arc<CaptureStats>,
) -> Result<(JoinHandle<()>, VideoInfo)> {
    let id = id.to_string();
    let (reader, width, height, subtype, stride) = {
        if ComInit::new().is_err() {
            return Err(anyhow!("COM init failed"));
        }
        mf_startup()?;
        let mut last_err = None;
        let mut out = None;
        for enable_processing in [false, true] {
            match create_source_reader(&id, enable_processing) {
                Ok(reader) => match configure_reader(&reader) {
                    Ok(cfg) => {
                        out = Some((reader, cfg));
                        break;
                    }
                    Err(e) => last_err = Some(e),
                },
                Err(e) => last_err = Some(e),
            }
        }
        let (reader, (width, height, subtype, stride)) = out
            .ok_or_else(|| last_err.unwrap_or_else(|| anyhow!("No supported media type")))?;
        (reader, width, height, subtype, stride)
    };
    let format = if subtype == MFVideoFormat_NV12 {
        "NV12"
    } else if subtype == MFVideoFormat_YUY2 {
        "YUY2"
    } else if subtype == MFVideoFormat_RGB32 {
        "RGB32"
    } else {
        "Unknown"
    };
    let info = VideoInfo {
        width,
        height,
        format: format.to_string(),
        fps: None,
    };
    let handle = std::thread::Builder::new()
        .name("mf-capture".to_string())
        .spawn(move || {
            if ComInit::new().is_err() {
                return;
            }
            if mf_startup().is_err() {
                return;
            }
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let stats_on = stats.enabled();
                let mut flags = 0u32;
                let mut sample = None;
                if reader
                    .ReadSample(
                        MF_SOURCE_READER_FIRST_VIDEO_STREAM,
                        0,
                        None,
                        Some(&mut flags),
                        None,
                        Some(&mut sample),
                    )
                    .is_err()
                {
                    continue;
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
                let Some(sample) = sample else { continue };
                if !drop_rx.is_empty() {
                    if stats_on {
                        stats.on_drop_enabled();
                    }
                    continue;
                }
                let buffer = match sample.ConvertToContiguousBuffer() {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let mut data_ptr = std::ptr::null_mut();
                let mut len = 0u32;
                if buffer.Lock(&mut data_ptr, None, Some(&mut len)).is_err() {
                    continue;
                }
                let data = std::slice::from_raw_parts(data_ptr, len as usize);
                let t0 = if stats_on { Some(std::time::Instant::now()) } else { None };
                let frame = if subtype == MFVideoFormat_NV12 {
                    VideoFrame {
                        width,
                        height,
                        format: VideoFormat::Nv12,
                        stride: stride as usize,
                        uv_stride: stride as usize,
                        color: ColorInfo::default_for_size(width),
                        data: FrameData::Owned(data.to_vec()),
                    }
                } else if subtype == MFVideoFormat_YUY2 {
                    VideoFrame {
                        width,
                        height,
                        format: VideoFormat::Yuyv,
                        stride: stride as usize,
                        uv_stride: 0,
                        color: ColorInfo::default_for_size(width),
                        data: FrameData::Owned(data.to_vec()),
                    }
                } else if subtype == MFVideoFormat_RGB32 {
                    let rgba = pixel::bgra_to_rgba(width, height, stride as usize, data);
                    VideoFrame {
                        width,
                        height,
                        format: VideoFormat::Rgba,
                        stride: (width * 4) as usize,
                        uv_stride: 0,
                        color: ColorInfo::default_for_size(width),
                        data: FrameData::Owned(rgba),
                    }
                } else {
                    let _ = buffer.Unlock();
                    continue;
                };
                let _ = buffer.Unlock();
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

fn mf_startup() -> Result<()> {
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_LITE).or_else(|e| {
            if e.code() == MF_E_ALREADY_INITIALIZED {
                Ok(())
            } else {
                Err(e)
            }
        })?;
    }
    Ok(())
}

fn create_attrs() -> Result<IMFAttributes> {
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.ok_or_else(|| anyhow!("No attributes"))?;
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        Ok(attrs)
    }
}

fn get_string(attrs: &IMFAttributes, key: &GUID) -> Result<String> {
    unsafe {
        let mut pwstr = PWSTR::null();
        let mut len = 0u32;
        attrs.GetAllocatedString(key, &mut pwstr, &mut len)?;
        let s = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as _));
        Ok(s)
    }
}

fn create_source_reader(id: &str, enable_processing: bool) -> Result<IMFSourceReader> {
    unsafe {
        let attrs = create_attrs()?;
        let mut activates = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut activates, &mut count)?;
        let slice = std::slice::from_raw_parts(activates, count as usize);
        let mut source = None;
        for act in slice.iter().flatten() {
            let sym = get_string(act, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)?;
            if sym == id {
                source = Some(act.ActivateObject::<IMFMediaSource>()?);
                break;
            }
        }
        CoTaskMemFree(Some(activates as _));
        let source = source.ok_or_else(|| anyhow!("Device not found"))?;
        let mut reader_attrs = None;
        MFCreateAttributes(&mut reader_attrs, 3)?;
        let reader_attrs = reader_attrs.ok_or_else(|| anyhow!("No reader attrs"))?;
        reader_attrs.SetUINT32(
            &MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
            enable_processing as u32,
        )?;
        let _ = reader_attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1);
        reader_attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;
        Ok(MFCreateSourceReaderFromMediaSource(&source, &reader_attrs)?)
    }
}

fn configure_reader(reader: &IMFSourceReader) -> Result<(u32, u32, GUID, u32)> {
    unsafe {
        let mut chosen = None;
        for subtype in [MFVideoFormat_NV12, MFVideoFormat_YUY2, MFVideoFormat_RGB32] {
            let mut mt = None;
            MFCreateMediaType(&mut mt)?;
            let mt = mt.ok_or_else(|| anyhow!("No media type"))?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            mt.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            if reader
                .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, None, &mt)
                .is_ok()
            {
                chosen = Some(subtype);
                break;
            }
        }
        let _ = chosen.ok_or_else(|| anyhow!("No supported media type"))?;
        let mt = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM)?;
        let mut size = 0u64;
        mt.GetUINT64(&MF_MT_FRAME_SIZE, &mut size)?;
        let width = (size >> 32) as u32;
        let height = size as u32;
        let mut subtype = GUID::default();
        mt.GetGUID(&MF_MT_SUBTYPE, &mut subtype)?;
        let mut stride = 0u32;
        if mt.GetUINT32(&MF_MT_DEFAULT_STRIDE, &mut stride).is_err() {
            stride = if subtype == MFVideoFormat_NV12 {
                width
            } else if subtype == MFVideoFormat_YUY2 {
                width * 2
            } else {
                width * 4
            };
        }
        Ok((width, height, subtype, stride))
    }
}

struct ComInit;

impl ComInit {
    fn new() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(anyhow!("CoInitializeEx failed: {hr:?}"));
        }
        Ok(Self)
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

pub struct KeepAwake;

impl KeepAwake {
    pub fn new() -> Option<Self> {
        let flags = ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED;
        let ok = unsafe { SetThreadExecutionState(flags) };
        if ok.0 == 0 {
            None
        } else {
            Some(Self)
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}
