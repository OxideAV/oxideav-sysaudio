//! Opaque [`Stream`] handle returned by `open()`.
//!
//! Each backend implements [`StreamImpl`] to provide a uniform control
//! surface (`play`, `pause`, `stop`) regardless of whether the backend
//! drives the callback from a worker thread (ALSA, PulseAudio) or from
//! an OS-owned audio thread (CoreAudio, WASAPI).

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
pub struct Stream {
    inner: Box<dyn StreamImpl>,
    stopped: bool,
}

impl Stream {
    pub(crate) fn new(inner: Box<dyn StreamImpl>) -> Self {
        Self {
            inner,
            stopped: false,
        }
    }

    pub fn play(&mut self) -> Result<()> {
        self.inner.play()
    }

    pub fn pause(&mut self) -> Result<()> {
        self.inner.pause()
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

impl Drop for Stream {
    fn drop(&mut self) {
        if !self.stopped {
            self.inner.stop();
            self.stopped = true;
        }
    }
}
