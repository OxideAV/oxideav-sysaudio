//! Opaque [`Stream`] handle returned by `open()`.
//!
//! Each backend implements [`StreamImpl`] to provide a uniform control
//! surface (`play`, `pause`, `stop`) regardless of whether the backend
//! drives the callback from a worker thread (ALSA, PulseAudio) or from
//! an OS-owned audio thread (CoreAudio, WASAPI).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::format::StreamFormat;
use crate::Result;

/// Internal per-backend stream implementation. Not public — the public
/// surface is [`Stream`] which type-erases this.
pub(crate) trait StreamImpl: Send {
    fn play(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn format(&self) -> StreamFormat;
    /// Output-side latency — how far behind real-time a sample is by
    /// the time it's submitted to the callback. Covers software
    /// buffering plus whatever the backend can see of the hardware
    /// pipeline (e.g. PulseAudio's network buffer, ALSA's driver
    /// queue, WASAPI's reported stream latency). Backends that can't
    /// measure it return `None`.
    ///
    /// A player uses this to compensate A/V sync when the output is a
    /// high-latency sink like Bluetooth: subtract `latency()` from the
    /// audio master clock to get the timestamp the user actually hears
    /// right now.
    fn latency(&self) -> Option<Duration> {
        None
    }
    /// Stop + release all resources. Called by `Stream::drop` and must
    /// be idempotent.
    fn stop(&mut self);
}

/// Handle to an open audio output stream. Dropping the handle stops the
/// stream and frees the device.
///
/// Streams start in the **playing** state: the callback begins running
/// as soon as `open()` returns (every backend behaves this way). Call
/// [`Stream::pause`] right after opening if you need to prepare a
/// stream ahead of time.
pub struct Stream {
    inner: Box<dyn StreamImpl>,
    stopped: bool,
    /// Last successfully *requested* transport state — see
    /// [`Stream::is_playing`].
    playing: bool,
    /// Software gain applied to the callback's output, stored as f32
    /// bits so the audio thread can read it wait-free. Shared with the
    /// wrapper closure `open()` installs around the user callback.
    volume: Arc<AtomicU32>,
}

impl Stream {
    pub(crate) fn new(inner: Box<dyn StreamImpl>, volume: Arc<AtomicU32>) -> Self {
        Self {
            inner,
            stopped: false,
            // Backends hand the stream over already running.
            playing: true,
            volume,
        }
    }

    pub fn play(&mut self) -> Result<()> {
        self.inner.play()?;
        self.playing = true;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        self.inner.pause()?;
        self.playing = false;
        Ok(())
    }

    /// The last transport state successfully requested through this
    /// handle: `true` from `open()` (streams start playing) and after a
    /// successful [`Stream::play`], `false` after a successful
    /// [`Stream::pause`] or once the stream is stopped.
    ///
    /// This tracks *requests*, not a hardware query — a device yanked
    /// mid-playback doesn't flip it. It answers "did I leave this
    /// stream playing or paused?", which is the state a player UI needs
    /// for its play/pause button.
    pub fn is_playing(&self) -> bool {
        self.playing && !self.stopped
    }

    /// Set the software volume applied to everything the callback
    /// renders, effective from the next audio-thread wakeup.
    ///
    /// `1.0` (the default) is unity gain and bypasses the multiply
    /// entirely; `0.0` is silence; values in between attenuate linearly
    /// (amplitude, not dB). Values above `1.0` amplify and may clip the
    /// backend's output range. Negative values and `NaN` clamp to
    /// `0.0`.
    ///
    /// This is a per-stream gain stage inside this crate — it does not
    /// touch the OS mixer volume, so it composes with (rather than
    /// fights) whatever the user set in their sound settings.
    pub fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// The current software volume, as last set by
    /// [`Stream::set_volume`] (after clamping). Defaults to `1.0`.
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn format(&self) -> StreamFormat {
        self.inner.format()
    }

    /// Current output latency, if the backend can measure it. See the
    /// `StreamImpl::latency` docs for interpretation — callers can
    /// subtract this from their audio clock to compensate Bluetooth /
    /// network sinks.
    pub fn latency(&self) -> Option<Duration> {
        self.inner.latency()
    }

    pub fn stop(mut self) {
        if !self.stopped {
            self.inner.stop();
            self.stopped = true;
        }
    }
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("format", &self.inner.format())
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if !self.stopped {
            self.inner.stop();
            self.stopped = true;
        }
    }
}
