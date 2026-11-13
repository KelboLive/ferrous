//! Every video encoder Ferrous knows how to drive, with the knobs each exposes.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Vendor {
    Software,
    Nvidia,
    Intel,
    Amd,
    MediaFoundation,
    /// Direct3D 12 Video encode: any GPU whose driver supports it.
    D3d12,
    /// Vulkan Video encode: any GPU whose driver supports it.
    Vulkan,
}

impl Vendor {
    pub fn label(self) -> &'static str {
        match self {
            Vendor::Software => "Software (CPU)",
            Vendor::Nvidia => "NVIDIA NVENC",
            Vendor::Intel => "Intel Quick Sync",
            Vendor::Amd => "AMD AMF",
            Vendor::MediaFoundation => "Windows Media Foundation",
            Vendor::D3d12 => "DirectX 12 Video",
            Vendor::Vulkan => "Vulkan Video",
        }
    }
    pub fn is_hardware(self) -> bool {
        !matches!(self, Vendor::Software)
    }
}

#[derive(Clone, Debug)]
pub struct EncoderInfo {
    /// FFmpeg encoder name.
    pub name: &'static str,
    pub codec: Codec,
    pub vendor: Vendor,
    pub presets: &'static [&'static str],
    pub default_preset: &'static str,
}

impl EncoderInfo {
    pub fn display_name(&self) -> String {
        format!("{} {} ({})", self.vendor.label(), self.codec.label(), self.name)
    }
}

const X26X_PRESETS: &[&str] = &[
    "ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow",
];
const NVENC_PRESETS: &[&str] = &["p1", "p2", "p3", "p4", "p5", "p6", "p7"];
const QSV_PRESETS: &[&str] = &["veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow"];
const AMF_PRESETS: &[&str] = &["speed", "balanced", "quality"];
const AMF_AV1_PRESETS: &[&str] = &["speed", "balanced", "quality", "high_quality"];
const SVT_PRESETS: &[&str] =
    &["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13"];
const AOM_PRESETS: &[&str] = &["0", "1", "2", "3", "4", "5", "6", "7", "8"];
const NONE: &[&str] = &[];

macro_rules! enc {
    ($name:literal, $codec:ident, $vendor:ident, $presets:expr, $default:literal) => {
        EncoderInfo {
            name: $name,
            codec: Codec::$codec,
            vendor: Vendor::$vendor,
            presets: $presets,
            default_preset: $default,
        }
    };
}

/// Ordered roughly by how most people should choose: hardware first.
pub static ENCODERS: &[EncoderInfo] = &[
    enc!("h264_nvenc", H264, Nvidia, NVENC_PRESETS, "p5"),
    enc!("hevc_nvenc", Hevc, Nvidia, NVENC_PRESETS, "p5"),
    enc!("av1_nvenc", Av1, Nvidia, NVENC_PRESETS, "p5"),
    enc!("h264_amf", H264, Amd, AMF_PRESETS, "quality"),
    enc!("hevc_amf", Hevc, Amd, AMF_PRESETS, "quality"),
    enc!("av1_amf", Av1, Amd, AMF_AV1_PRESETS, "quality"),
    enc!("h264_qsv", H264, Intel, QSV_PRESETS, "medium"),
    enc!("hevc_qsv", Hevc, Intel, QSV_PRESETS, "medium"),
    enc!("av1_qsv", Av1, Intel, QSV_PRESETS, "medium"),
    enc!("h264_d3d12va", H264, D3d12, NONE, ""),
    enc!("hevc_d3d12va", Hevc, D3d12, NONE, ""),
    enc!("av1_d3d12va", Av1, D3d12, NONE, ""),
    enc!("h264_vulkan", H264, Vulkan, NONE, ""),
    enc!("hevc_vulkan", Hevc, Vulkan, NONE, ""),
    enc!("av1_vulkan", Av1, Vulkan, NONE, ""),
    enc!("h264_mf", H264, MediaFoundation, NONE, ""),
    enc!("hevc_mf", Hevc, MediaFoundation, NONE, ""),
    enc!("libx264", H264, Software, X26X_PRESETS, "veryfast"),
    enc!("libx265", Hevc, Software, X26X_PRESETS, "fast"),
    enc!("libsvtav1", Av1, Software, SVT_PRESETS, "8"),
    enc!("libaom-av1", Av1, Software, AOM_PRESETS, "6"),
];

pub fn find(name: &str) -> Option<&'static EncoderInfo> {
    ENCODERS.iter().find(|e| e.name == name)
}
