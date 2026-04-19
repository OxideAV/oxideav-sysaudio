//! Pure-Rust audio output with runtime-loaded native backends.
//!
//! `oxideav-sysaudio` talks to the platform's audio stack through
//! `libloading` instead of linking against audio system libraries at
//! build time. The produced binary has no `libasound`, `libpulse`,
//! `ole32`, or `AudioToolbox` entry in its NEEDED list — everything is
//! `dlopen`'d on first use and the backends fall through to each other
//! gracefully if a library is missing.
//!
//! # Quick start
//!
//! ```no_run
//! use oxideav_sysaudio::{open_default, StreamRequest};
//!
//! let req = StreamRequest::new(48_000, 2);
//! let mut stream = open_default(req, |out, _info| {
//!     // Fill `out` with interleaved f32 samples in [-1.0, 1.0].
//!     out.fill(0.0);
//! })
//! .expect("no audio driver available");
//! stream.play().ok();
//! // ... audio plays for the lifetime of `stream` ...
//! ```
//!
//! # Driver probing and explicit selection
//!
//! [`probe()`] returns the subset of compiled-in backends whose shared
//! library loads and whose device opens cleanly, ordered by platform
//! preference. [`default_driver()`] is the first entry of that list.
//! Callers that need to force a specific backend pass a [`Driver`] to
//! [`open()`]; [`drivers()`] enumerates every compiled-in backend
//! (including ones whose library isn't installed).

mod backend;
mod backends;
mod error;
mod format;
mod stream;

pub use error::{Error, Result};
pub use format::{CallbackInfo, SampleFormat, StreamFormat, StreamRequest};
pub use stream::Stream;

use backend::Backend;

/// Public, opaque handle to a backend. Call [`drivers()`] or
/// [`probe()`] to get one; pass it to [`open()`] to open a stream.
#[derive(Clone, Copy)]
pub struct Driver {
    inner: &'static dyn Backend,
}

impl Driver {
    pub fn name(&self) -> &'static str {
        self.inner.name()
    }

    pub fn description(&self) -> &'static str {
        self.inner.description()
    }
}

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("name", &self.name())
            .finish()
    }
}

impl PartialEq for Driver {
    fn eq(&self, other: &Self) -> bool {
        // Two backends are equal iff they point at the same static
        // singleton. Backend names are unique per target_os so comparing
        // by name is equivalent; doing the pointer comparison is a
        // cheap invariant check against accidental duplicates.
        std::ptr::eq(
            self.inner as *const _ as *const (),
            other.inner as *const _ as *const (),
        )
    }
}

impl Eq for Driver {}

/// Every backend compiled in for the current target_os, including
/// stubs and backends whose shared library isn't installed. For the
/// "ready to use now" list, call [`probe()`].
pub fn drivers() -> Vec<Driver> {
    backends::drivers()
        .iter()
        .map(|b| Driver { inner: *b })
        .collect()
}

/// Backends whose library loads and whose device opens cleanly, in the
/// platform's preferred order. First entry is what [`default_driver()`]
/// returns. Errors during probing are swallowed — a failed backend is
/// simply absent from the result.
pub fn probe() -> Vec<Driver> {
    backends::drivers()
        .iter()
        .filter(|b| b.probe().is_ok())
        .map(|b| Driver { inner: *b })
        .collect()
}

/// First driver returned by [`probe()`], or `None` if nothing works.
pub fn default_driver() -> Option<Driver> {
    probe().into_iter().next()
}

/// Look up a backend by its short name (`"alsa"`, `"pulse"`,
/// `"wasapi"`, …). Unlike [`probe()`] this does not attempt to open the
/// device — the caller gets a handle even for backends that will later
/// fail at `open()`. Returns `None` if no compiled-in backend has that
/// name.
pub fn driver_by_name(name: &str) -> Option<Driver> {
    backends::drivers()
        .iter()
        .find(|b| b.name() == name)
        .map(|b| Driver { inner: *b })
}

/// Open an output stream on the given backend. The callback is invoked
/// from a backend-owned thread with f32 interleaved samples; fill it
/// with audio to play, or write zeros for silence.
pub fn open<F>(driver: Driver, req: StreamRequest, cb: F) -> Result<Stream>
where
    F: FnMut(&mut [f32], &CallbackInfo) + Send + 'static,
{
    let inner = driver.inner.open(req, Box::new(cb))?;
    Ok(Stream::new(inner))
}

/// Convenience wrapper for `open(default_driver(), …)`.
pub fn open_default<F>(req: StreamRequest, cb: F) -> Result<Stream>
where
    F: FnMut(&mut [f32], &CallbackInfo) + Send + 'static,
{
    let d = default_driver()
        .ok_or_else(|| Error::NoDriver("probe() returned no working backend".into()))?;
    open(d, req, cb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drivers_non_empty_per_platform() {
        // Every platform we support should have at least one compiled-in
        // backend — even if all are stubs, the list shouldn't be empty.
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        assert!(!drivers().is_empty());
    }

    #[test]
    fn driver_by_name_roundtrip() {
        for d in drivers() {
            assert_eq!(driver_by_name(d.name()).map(|x| x.name()), Some(d.name()));
        }
    }
}
