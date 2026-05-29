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
#[derive(Debug, Clone)]
pub struct StreamRequest {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
    /// Requested buffer size in frames (one frame = `channels` samples).
    /// `None` lets the backend pick. Backends treat this as a hint, not
    /// a constraint.
    pub buffer_frames: Option<u32>,
    /// Bind the stream to a specific enumerated device, identified by the
    /// opaque `id` returned in [`Device::id`] by the same backend's
    /// [`crate::Driver::output_devices`] call.
    ///
    /// `None` (the default) opens the system default endpoint — the same
    /// behaviour as [`crate::open_default`]. When `Some(id)`, the backend
    /// resolves `id` to the matching device and opens against it. Each
    /// backend interprets `id` in its native format:
    ///
    /// - **ALSA**: passed straight to `snd_pcm_open` as the PCM name
    ///   (e.g. `"plughw:CARD=PCH,DEV=0"` or `"default"`).
    /// - **PulseAudio**: passed as the `dev` argument of `pa_simple_new`
    ///   (a sink name); the simple API doesn't enumerate so the caller
    ///   typically obtains the id out-of-band via `pactl list sinks short`.
    /// - **WASAPI**: resolved through `IMMDeviceEnumerator::GetDevice`
    ///   against the LPWSTR endpoint id `output_devices()` returned.
    /// - **CoreAudio**: not yet wired — see crate README "Non-goals". The
    ///   `id` we expose is the numeric `AudioDeviceID`, but routing an
    ///   AudioQueue at a specific device requires the device UID
    ///   (CFStringRef) and CoreFoundation symbol resolution we have not
    ///   added yet. Setting `device` on macOS currently returns
    ///   [`crate::Error::UnsupportedFormat`].
    ///
    /// Passing an id that the backend can't resolve surfaces as a normal
    /// [`crate::Error::DeviceOpen`].
    pub device: Option<String>,
}

impl StreamRequest {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
            format: SampleFormat::F32,
            buffer_frames: None,
            device: None,
        }
    }

    /// Bind this request to a specific enumerated device, identified by
    /// the opaque `id` from [`Device::id`]. See [`StreamRequest::device`]
    /// for per-backend semantics.
    pub fn with_device(mut self, id: impl Into<String>) -> Self {
        self.device = Some(id.into());
        self
    }

    /// Override the hinted buffer size (period in frames). `None` lets
    /// the backend pick its own ~20 ms default.
    pub fn with_buffer_frames(mut self, frames: Option<u32>) -> Self {
        self.buffer_frames = frames;
        self
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
