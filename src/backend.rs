//! Internal backend trait. Each module under `backends/` implements
//! this; the public API iterates over a static slice of `&dyn Backend`.

use crate::format::{CallbackInfo, Device, StreamFormat, StreamRequest};
use crate::stream::StreamImpl;
use crate::{Error, Result};

pub(crate) type Callback = Box<dyn FnMut(&mut [f32], &CallbackInfo) + Send + 'static>;

pub(crate) trait Backend: Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// Compile-time capability flag: `true` for backends whose
    /// implementation is still a placeholder (PipeWire, ASIO) — every
    /// call into them will fail with [`Error::NotImplemented`] regardless
    /// of host configuration. Default `false` for the functional
    /// backends. Used by [`crate::Driver::is_stub`] so callers can
    /// distinguish "backend not implemented yet" from "backend
    /// implemented but the shared library isn't installed on this host".
    fn is_stub(&self) -> bool {
        false
    }

    /// Cheap check that this backend is usable right now: loads the
    /// shared library and opens a throw-away handle. Errors are
    /// swallowed by `probe()`; they don't bubble to the user.
    fn probe(&self) -> Result<()>;

    /// Open a real output stream. Returns a boxed [`StreamImpl`] the
    /// caller wraps in [`crate::Stream`].
    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>>;

    /// Enumerate the playback (output) devices this backend can see,
    /// each tagged with whether it is the system default. Backends that
    /// only know how to reach the default device (the PulseAudio
    /// "simple" API, stubs) keep the default below and surface
    /// [`Error::NotImplemented`]; the public layer maps that into an
    /// empty list rather than an error so callers can union device lists
    /// across drivers without special-casing.
    fn output_devices(&self) -> Result<Vec<Device>> {
        Err(Error::NotImplemented(self.name()))
    }

    /// Best-effort report of the device a plain `open()` (with no
    /// `with_device`) would currently bind to — the system's default
    /// output endpoint. The base implementation is a thin reduction over
    /// [`Backend::output_devices`]: take the one entry whose
    /// [`Device::is_default`] is set. Backends that have a cheaper
    /// direct-query path (e.g. CoreAudio's
    /// `kAudioHardwarePropertyDefaultOutputDevice` system-object
    /// selector, WASAPI's `IMMDeviceEnumerator::GetDefaultAudioEndpoint`)
    /// may override this with the direct call later; the reduction is
    /// the always-correct fallback because every backend that enumerates
    /// already tags the default entry.
    ///
    /// Returns `Err(Error::NotImplemented(_))` when the backend has no
    /// enumeration path at all (the PulseAudio "simple" API + the
    /// not-yet-wired stubs); the public layer maps that into `Ok(None)`
    /// so a caller can union per-driver results without per-backend
    /// special-casing. Returns `Ok(None)` from an enumerating backend
    /// when the list is empty (no playback devices currently visible to
    /// the OS — unplugged USB DAC, no built-in speakers on a headless VM)
    /// or no entry is flagged as default (the OS itself reports no
    /// default endpoint, which can happen between hotplug events on
    /// CoreAudio).
    fn default_output_device(&self) -> Result<Option<Device>> {
        Ok(self.output_devices()?.into_iter().find(|d| d.is_default))
    }

    /// Best-effort query of what `open()` would actually agree on for the
    /// system default (`device_id == None`) or the named endpoint, without
    /// committing a live stream. Returned `sample_rate` is what the
    /// backend would settle on for an unconstrained request — the mix
    /// engine's preferred rate on WASAPI, the device's nominal sample
    /// rate on CoreAudio, the rate `snd_pcm_hw_params_set_rate_near`
    /// snaps to on ALSA. Callers use it to resample their input ahead of
    /// `open()` so the backend doesn't end up doing a hidden software
    /// conversion. Backends that can't introspect (the PulseAudio
    /// "simple" API, the not-yet-wired stubs) surface
    /// [`Error::NotImplemented`]; the public layer maps that into `None`
    /// so a caller can iterate every driver without per-backend
    /// special-casing.
    fn preferred_format(&self, _device_id: Option<&str>) -> Result<StreamFormat> {
        Err(Error::NotImplemented(self.name()))
    }
}
