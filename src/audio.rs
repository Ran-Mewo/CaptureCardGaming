use anyhow::Result;

use crate::types::DeviceInfo;

#[cfg(target_os = "linux")]
mod gst_audio {
    use super::*;
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use std::collections::HashSet;

    pub struct AudioDevice {
        pub info: DeviceInfo,
        device: gst::Device,
        pipewire_target: Option<String>,
    }

    pub struct AudioPlayback {
        pipeline: gst::Pipeline,
    }

    impl Drop for AudioPlayback {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    pub fn list_input_devices() -> Result<Vec<AudioDevice>> {
        gst::init()?;
        let monitor = gst::DeviceMonitor::new();
        let caps = gst::Caps::builder("audio/x-raw").build();
        let _ = monitor.add_filter(Some("Audio/Source"), Some(&caps));
        monitor.start()?;
        let devices = monitor.devices();
        monitor.stop();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (i, dev) in devices.into_iter().enumerate() {
            let name = dev.display_name().to_string();
            if dev.device_class().contains("/Virtual") {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            let pipewire_target = pipewire_target_from_props(&dev);
            out.push(AudioDevice {
                info: DeviceInfo {
                    id: i.to_string(),
                    name,
                },
                device: dev,
                pipewire_target,
            });
        }
        Ok(out)
    }

    fn set_i64_prop(elem: &gst::Element, name: &str, value: i64) {
        if elem.find_property(name).is_some() {
            elem.set_property(name, value);
        }
    }

    fn set_bool_prop(elem: &gst::Element, name: &str, value: bool) {
        if elem.find_property(name).is_some() {
            elem.set_property(name, value);
        }
    }

    fn apply_low_latency(elem: &gst::Element) {
        set_i64_prop(elem, "latency-time", 10_000);
        set_i64_prop(elem, "buffer-time", 20_000);
    }

    fn make_audio_sink() -> Result<gst::Element> {
        let sink = if gst::ElementFactory::find("pipewiresink").is_some() {
            gst::ElementFactory::make("pipewiresink").build()?
        } else if gst::ElementFactory::find("pulsesink").is_some() {
            gst::ElementFactory::make("pulsesink").build()?
        } else if gst::ElementFactory::find("alsasink").is_some() {
            gst::ElementFactory::make("alsasink").build()?
        } else {
            gst::ElementFactory::make("autoaudiosink").build()?
        };
        set_bool_prop(&sink, "sync", false);
        apply_low_latency(&sink);
        Ok(sink)
    }

    pub fn start_playback(device: &AudioDevice) -> Result<AudioPlayback> {
        gst::init()?;
        let pipeline = gst::Pipeline::new();
        let src = if let Some(target) = device.pipewire_target.as_ref() {
            if gst::ElementFactory::find("pipewiresrc").is_some() {
                let src = gst::ElementFactory::make("pipewiresrc").build()?;
                src.set_property("target-object", target);
                src
            } else {
                device.device.create_element(Some("audiosrc"))?
            }
        } else {
            device.device.create_element(Some("audiosrc"))?
        };
        set_bool_prop(&src, "do-timestamp", true);
        apply_low_latency(&src);
        let queue = gst::ElementFactory::make("queue").build()?;
        queue.set_property_from_str("leaky", "downstream");
        queue.set_property_from_str("max-size-buffers", "1");
        queue.set_property_from_str("max-size-time", "0");
        queue.set_property_from_str("max-size-bytes", "0");
        let convert = gst::ElementFactory::make("audioconvert").build()?;
        let resample = gst::ElementFactory::make("audioresample").build()?;
        if resample.find_property("quality").is_some() {
            resample.set_property("quality", 0i32);
        }
        let sink = make_audio_sink()?;
        pipeline.add_many([&src, &queue, &convert, &resample, &sink])?;
        gst::Element::link_many([&src, &queue, &convert, &resample, &sink])?;
        pipeline.set_state(gst::State::Playing)?;
        Ok(AudioPlayback { pipeline })
    }

    fn pipewire_target_from_props(device: &gst::Device) -> Option<String> {
        let props = device.properties()?;
        if let Ok(serial) = props.get::<i64>("object.serial") {
            return Some(serial.to_string());
        }
        if let Ok(serial) = props.get::<u64>("object.serial") {
            return Some(serial.to_string());
        }
        if let Ok(serial) = props.get::<i32>("object.serial") {
            return Some(serial.to_string());
        }
        if let Ok(serial) = props.get::<u32>("object.serial") {
            return Some(serial.to_string());
        }
        if let Ok(name) = props.get::<String>("node.name") {
            return Some(name);
        }
        None
    }
}

#[cfg(target_os = "windows")]
mod wasapi_audio {
    use super::*;
    use anyhow::anyhow;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread::JoinHandle;
    use windows::core::{HSTRING, Interface, PWSTR};
    use windows::Win32::Foundation::*;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::Media::Audio::Endpoints::*;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Com::StructuredStorage::*;
    use windows::Win32::System::Threading::*;
    use windows::Win32::UI::Shell::PropertiesSystem::*;

    pub struct AudioDevice {
        pub info: DeviceInfo,
    }

    pub struct AudioPlayback {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for AudioPlayback {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.thread.take() {
                let _ = handle.join();
            }
        }
    }

    pub fn list_input_devices() -> Result<Vec<AudioDevice>> {
        let _com = ComInit::new()?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;
        let mut out = Vec::new();
        for i in 0..count {
            let device = collection.Item(i)?;
            let id = device_id(&device)?;
            let name = friendly_name(&device).unwrap_or_else(|| id.clone());
            out.push(AudioDevice {
                info: DeviceInfo { id, name },
            });
        }
        Ok(out)
    }

    pub fn start_playback(device: &AudioDevice) -> Result<AudioPlayback> {
        let id = device.info.id.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let stop_thread = stop.clone();
        let handle = std::thread::Builder::new()
            .name("wasapi-audio".to_string())
            .spawn(move || {
                let res = run_wasapi(&id, stop_thread);
                let _ = ready_tx.send(res);
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(AudioPlayback {
                stop,
                thread: Some(handle),
            }),
            Ok(Err(e)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                let _ = handle.join();
                Err(anyhow!("Audio thread failed"))
            }
        }
    }

    fn run_wasapi(id: &str, stop: Arc<AtomicBool>) -> Result<()> {
        let _com = ComInit::new()?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let capture_device = enumerator.GetDevice(&HSTRING::from(id))?;
        let render_device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let capture_client: IAudioClient =
            capture_device.Activate(CLSCTX_ALL, None)?;
        let render_client: IAudioClient =
            render_device.Activate(CLSCTX_ALL, None)?;
        let format = select_format(&capture_client, &render_client)?;
        let buffer_duration = 100_000;
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_NOPERSIST;
        unsafe {
            capture_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                buffer_duration,
                0,
                format.as_ptr(),
                std::ptr::null(),
            )?;
            render_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                buffer_duration,
                0,
                format.as_ptr(),
                std::ptr::null(),
            )?;
        }
        let render_frames = render_client.GetBufferSize()?;
        let capture_event = unsafe { CreateEventW(None, false, false, None)? };
        let _render_event = unsafe { CreateEventW(None, false, false, None)? };
        capture_client.SetEventHandle(capture_event)?;
        render_client.SetEventHandle(_render_event)?;
        let capture: IAudioCaptureClient = capture_client.GetService()?;
        let render: IAudioRenderClient = render_client.GetService()?;
        unsafe {
            render_client.Start()?;
            capture_client.Start()?;
        }
        let frame_size = format.block_align();
        while !stop.load(Ordering::Relaxed) {
            let wait = unsafe { WaitForSingleObject(capture_event, 50) };
            if wait != WAIT_OBJECT_0 {
                continue;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            unsafe {
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            }
            if frames == 0 {
                unsafe { capture.ReleaseBuffer(0)? };
                continue;
            }
            let padding = render_client.GetCurrentPadding()?;
            let available = render_frames.saturating_sub(padding);
            let write_frames = frames.min(available);
            if write_frames > 0 {
                let mut out = std::ptr::null_mut();
                unsafe {
                    render.GetBuffer(write_frames, &mut out)?;
                    let bytes = write_frames as usize * frame_size;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        std::ptr::write_bytes(out, 0, bytes);
                    } else {
                        std::ptr::copy_nonoverlapping(data, out, bytes);
                    }
                    render.ReleaseBuffer(write_frames, 0)?;
                }
            }
            unsafe {
                capture.ReleaseBuffer(frames)?;
            }
        }
        unsafe {
            let _ = capture_client.Stop();
            let _ = render_client.Stop();
        }
        Ok(())
    }

    fn select_format(
        capture: &IAudioClient,
        render: &IAudioClient,
    ) -> Result<WaveFormat> {
        let mix = unsafe { render.GetMixFormat()? };
        let mix_fmt = unsafe { WaveFormat::from_ptr(mix) };
        unsafe { CoTaskMemFree(Some(mix as _)) };
        if supports_format(capture, &mix_fmt) && supports_format(render, &mix_fmt) {
            return Ok(mix_fmt);
        }
        for fmt in preferred_formats() {
            if supports_format(capture, &fmt) && supports_format(render, &fmt) {
                return Ok(fmt);
            }
        }
        Err(anyhow!("No shared audio format"))
    }

    fn supports_format(client: &IAudioClient, fmt: &WaveFormat) -> bool {
        let mut closest = std::ptr::null_mut();
        let ok =
            unsafe { client.IsFormatSupported(AUDCLNT_SHAREMODE_SHARED, fmt.as_ptr(), Some(&mut closest)) }
                .is_ok();
        if !closest.is_null() {
            unsafe { CoTaskMemFree(Some(closest as _)) };
        }
        ok
    }

    fn preferred_formats() -> Vec<WaveFormat> {
        let mut out = Vec::new();
        for &(rate, ch, bits, float) in &[
            (48_000, 2, 32, true),
            (48_000, 2, 16, false),
            (44_100, 2, 32, true),
            (44_100, 2, 16, false),
            (48_000, 1, 32, true),
            (48_000, 1, 16, false),
        ] {
            out.push(WaveFormat::new(rate, ch, bits, float));
        }
        out
    }

    struct WaveFormat {
        data: Vec<u8>,
    }

    impl WaveFormat {
        fn new(rate: u32, channels: u16, bits: u16, float: bool) -> Self {
            let mut fmt = WAVEFORMATEX::default();
            fmt.wFormatTag = if float {
                WAVE_FORMAT_IEEE_FLOAT as u16
            } else {
                WAVE_FORMAT_PCM as u16
            };
            fmt.nChannels = channels;
            fmt.nSamplesPerSec = rate;
            fmt.wBitsPerSample = bits;
            fmt.nBlockAlign = (channels * (bits / 8)) as u16;
            fmt.nAvgBytesPerSec = rate * fmt.nBlockAlign as u32;
            fmt.cbSize = 0;
            let mut data = Vec::with_capacity(std::mem::size_of::<WAVEFORMATEX>());
            unsafe {
                data.set_len(std::mem::size_of::<WAVEFORMATEX>());
                std::ptr::copy_nonoverlapping(
                    &fmt as *const WAVEFORMATEX as *const u8,
                    data.as_mut_ptr(),
                    data.len(),
                );
            }
            Self { data }
        }

        unsafe fn from_ptr(ptr: *const WAVEFORMATEX) -> Self {
            let size = std::mem::size_of::<WAVEFORMATEX>() + (*ptr).cbSize as usize;
            let mut data = Vec::with_capacity(size);
            data.set_len(size);
            std::ptr::copy_nonoverlapping(ptr as *const u8, data.as_mut_ptr(), size);
            Self { data }
        }

        fn as_ptr(&self) -> *const WAVEFORMATEX {
            self.data.as_ptr() as *const WAVEFORMATEX
        }

        fn block_align(&self) -> usize {
            unsafe { (*self.as_ptr()).nBlockAlign as usize }
        }
    }

    fn device_id(device: &IMMDevice) -> Result<String> {
        unsafe {
            let mut id = PWSTR::null();
            device.GetId(&mut id)?;
            let s = id.to_string().unwrap_or_default();
            CoTaskMemFree(Some(id.0 as _));
            Ok(s)
        }
    }

    fn friendly_name(device: &IMMDevice) -> Option<String> {
        unsafe {
            let store = device.OpenPropertyStore(STGM_READ).ok()?;
            let mut pv = PROPVARIANT::default();
            if store.GetValue(&PKEY_Device_FriendlyName, &mut pv).is_err() {
                return None;
            }
            let mut out = PWSTR::null();
            if PropVariantToStringAlloc(&pv, &mut out).is_err() {
                let _ = PropVariantClear(&mut pv);
                return None;
            }
            let name = out.to_string().ok();
            CoTaskMemFree(Some(out.0 as _));
            let _ = PropVariantClear(&mut pv);
            name
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
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod stub_audio {
    use super::*;
    use anyhow::anyhow;

    pub struct AudioDevice {
        pub info: DeviceInfo,
    }

    pub struct AudioPlayback;

    pub fn list_input_devices() -> Result<Vec<AudioDevice>> {
        Ok(Vec::new())
    }

    pub fn start_playback(_: &AudioDevice) -> Result<AudioPlayback> {
        Err(anyhow!("Audio capture unsupported on this platform"))
    }
}

#[cfg(target_os = "linux")]
pub use gst_audio::{list_input_devices, start_playback, AudioDevice, AudioPlayback};
#[cfg(target_os = "windows")]
pub use wasapi_audio::{list_input_devices, start_playback, AudioDevice, AudioPlayback};
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub use stub_audio::{list_input_devices, start_playback, AudioDevice, AudioPlayback};
