#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoFormat {
    Rgba,
    Yuyv,
    Nv12,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorRange {
    Limited,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    pub matrix: ColorMatrix,
    pub range: ColorRange,
}

impl ColorInfo {
    pub fn default_for_size(width: u32) -> Self {
        let matrix = if width >= 1280 {
            ColorMatrix::Bt709
        } else {
            ColorMatrix::Bt601
        };
        Self {
            matrix,
            range: ColorRange::Limited,
        }
    }
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self {
            matrix: ColorMatrix::Bt709,
            range: ColorRange::Limited,
        }
    }
}

#[derive(Clone, Debug)]
pub enum FrameData {
    Owned(Vec<u8>),
    #[cfg(target_os = "linux")]
    Gst(gstreamer::Buffer),
}

#[derive(Clone, Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: VideoFormat,
    pub stride: usize,
    pub uv_stride: usize,
    pub color: ColorInfo,
    pub data: FrameData,
}
