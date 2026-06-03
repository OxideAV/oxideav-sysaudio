//! Internal backend trait. Each module under `backends/` implements
//! this; the public API iterates over a static slice of `&dyn Backend`.

use crate::format::{CallbackInfo, Device, StreamFormat, StreamRequest};
use crate::stream::StreamImpl;
use crate::{Error, Result};

pub(crate) type Callback = Box<dyn FnMut(&mut [f32], &CallbackInfo) + Send + 'static>;

pub(crate) trait Backend: Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

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
