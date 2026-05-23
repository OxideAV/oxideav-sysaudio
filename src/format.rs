//! Sample-format types exposed on the public API.
//!
//! The callback always receives f32 interleaved samples at the device's
//! sample rate. Backends convert internally when the device wants
//! something else (e.g. S16LE on very old ALSA cards).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// 32-bit IEEE 754 float, native endian, interleaved. The only
    /// format the public callback API currently speaks.
    F32,
}

/// Caller-supplied preferred format. Backends may return a different
/// actual format in [`StreamFormat`] if the device can't honor the
/// request exactly.
#[derive(Debug, Clone, Copy)]
pub struct StreamRequest {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
    /// Requested buffer size in frames (one frame = `channels` samples).
    /// `None` lets the backend pick. Backends treat this as a hint, not
    /// a constraint.
    pub buffer_frames: Option<u32>,
}

impl StreamRequest {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
            format: SampleFormat::F32,
            buffer_frames: None,
        }
    }
}

/// Actual stream parameters after the backend has reconciled the
/// request with what the device supports.
#[derive(Debug, Clone, Copy)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

/// Information passed to the callback alongside the output buffer.
#[derive(Debug, Clone, Copy)]
pub struct CallbackInfo {
    /// Monotonic count of frames played on this stream. Useful as a
    /// cheap audio master clock.
    pub frames_played: u64,
}

/// A single output device discovered by a backend's enumeration.
///
/// Returned by [`crate::Driver::output_devices`] /
/// [`crate::output_devices`]. The `id` is the backend-native opaque
/// identifier (an ALSA PCM name like `"plughw:CARD=PCH,DEV=0"`, a WASAPI
/// endpoint id wstring rendered as UTF-8, a CoreAudio numeric
/// `AudioDeviceID` formatted as decimal) and is stable enough to log or
/// match against; `name` is the human-friendly label the OS shows in its
/// sound settings. Exactly one device in a non-empty list has
/// `is_default == true` — the one [`crate::default_driver`] / `open()`
/// would use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Backend-native opaque identifier. Format differs per backend; do
    /// not parse it — treat it as an opaque token for logging or
    /// equality comparison against another `Device` from the same
    /// backend.
    pub id: String,
    /// Human-readable name as shown in the OS sound settings (e.g.
    /// "MacBook Pro Speakers", "Realtek HD Audio Output", "USB
    /// Headset"). May be empty if the OS exposes no friendly label.
    pub name: String,
    /// `true` for the system's current default output endpoint — the
    /// device a plain `open_default()` plays through.
    pub is_default: bool,
}
