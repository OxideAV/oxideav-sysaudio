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
pub use format::{CallbackInfo, Device, SampleFormat, StreamFormat, StreamRequest};
pub use stream::Stream;

/// Test-support helpers for the virtual `"mock"` backend (cargo
/// feature `mock`). See `backends::mock` module docs for the
/// behavioural model; the backend itself is reached through the normal
/// driver surface (`driver_by_name("mock")`).
#[cfg(feature = "mock")]
pub mod mock {
    pub use crate::backends::mock::take_captured;
}

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

    /// `true` when this backend is compiled in as a placeholder rather
    /// than a working implementation — currently PipeWire on Linux and
    /// ASIO on Windows, both tracked as follow-ups in the README. Every
    /// call into a stub backend ([`Driver::output_devices`],
    /// [`crate::open`], …) fails with the `NotImplemented` family of
    /// errors regardless of host configuration. Functional backends
    /// return `false`.
    ///
    /// Useful for callers iterating [`drivers()`] (which includes stubs)
    /// who want to surface "PipeWire — not yet implemented" in a UI
    /// distinctly from "ALSA — library not installed on this host". The
    /// latter shows up in [`drivers()`] but not in [`probe()`]; the
    /// former shows up in [`drivers()`] but cannot be told apart from a
    /// missing library by [`probe()`] alone (both fail the probe). This
    /// accessor closes that gap with a compile-time bit on each
    /// backend, independent of probe outcome.
    ///
    /// ```no_run
    /// for d in oxideav_sysaudio::drivers() {
    ///     let tag = if d.is_stub() { " (stub)" } else { "" };
    ///     println!("{}: {}{tag}", d.name(), d.description());
    /// }
    /// ```
    pub fn is_stub(&self) -> bool {
        self.inner.is_stub()
    }

    /// Enumerate this backend's playback (output) devices, each tagged
    /// with whether it is the system default. The list is in the
    /// backend's natural order; exactly one entry has
    /// [`Device::is_default`] set when the backend can identify a
    /// default (and the list is non-empty).
    ///
    /// Backends that can only reach the default device — the PulseAudio
    /// "simple" API, and the not-yet-wired stubs (PipeWire, OSS, ASIO) —
    /// return an empty list rather than an error, so a caller can union
    /// device lists across every probed driver without per-backend
    /// special-casing. A genuine failure (library present but the
    /// enumeration call errored) surfaces as an [`Err`].
    pub fn output_devices(&self) -> Result<Vec<Device>> {
        match self.inner.output_devices() {
            Ok(v) => Ok(v),
            // A backend that doesn't implement enumeration is reported
            // as "no devices known", not an error — see the trait doc.
            Err(Error::NotImplemented(_)) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// The single playback device a plain [`crate::open`] (with no
    /// `with_device`) would bind to — the system's default output
    /// endpoint. Equivalent to scanning [`Driver::output_devices`] for
    /// the entry whose [`Device::is_default`] flag is set, but exposed
    /// as a one-call shortcut so a caller only interested in "where does
    /// the system play right now?" doesn't have to materialise the full
    /// device list and filter.
    ///
    /// Returns `Ok(None)` rather than an error when the backend has no
    /// enumeration path at all (the PulseAudio "simple" API and the
    /// not-yet-wired PipeWire/OSS/ASIO stubs), so callers can union
    /// per-driver results without per-backend special-casing. Also
    /// returns `Ok(None)` from an enumerating backend when no device is
    /// currently flagged as default — the list was empty (no playback
    /// hardware visible to the OS) or the OS itself reports no default
    /// endpoint (a transient state on CoreAudio between hotplug events).
    ///
    /// The `id` in the returned [`Device`] is the same backend-native
    /// opaque token [`Driver::output_devices`] uses, suitable for
    /// feeding back into [`crate::StreamRequest::with_device`] /
    /// [`crate::open_on`].
    pub fn default_output_device(&self) -> Result<Option<Device>> {
        match self.inner.default_output_device() {
            Ok(v) => Ok(v),
            // A backend that doesn't enumerate is reported as "no
            // default device known", not an error — see the trait doc.
            Err(Error::NotImplemented(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Best-effort query of the [`StreamFormat`] this backend would settle
    /// on if [`crate::open`] were called against `device` (or the system
    /// default when `device` is `None`). The returned `sample_rate` is the
    /// rate the backend would actually run at — the WASAPI mix engine's
    /// preferred rate (often 48 kHz on Windows-10/11, sometimes 44.1 kHz
    /// on older endpoints), the CoreAudio HAL's `NominalSampleRate`, the
    /// rate ALSA's `snd_pcm_hw_params_set_rate_near` snaps the unconstrained
    /// request to. Callers use it to plan resampling before
    /// [`crate::open`] so the OS mix engine doesn't end up doing a hidden
    /// software conversion. `channels` and `format` are similarly
    /// best-effort.
    ///
    /// Returns `Ok(None)` rather than an error when the backend has no
    /// introspection path (the PulseAudio "simple" API, the
    /// PipeWire/OSS/ASIO stubs) — callers can union per-driver results
    /// without per-backend special-casing.
    ///
    /// `device` must belong to the same `Driver` it was enumerated from;
    /// passing a foreign id surfaces the backend's own error path
    /// (typically [`Error::DeviceOpen`] / [`Error::UnsupportedFormat`]).
    pub fn preferred_format(&self, device: Option<&Device>) -> Result<Option<StreamFormat>> {
        let id = device.map(|d| d.id.as_str());
        match self.inner.preferred_format(id) {
            Ok(f) => Ok(Some(f)),
            // A backend that doesn't introspect is reported as "no
            // preferred format known", not an error — see the trait doc.
            Err(Error::NotImplemented(_)) => Ok(None),
            Err(e) => Err(e),
        }
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

/// Reject requests no backend could ever satisfy, before any library
/// gets loaded or device touched. Keeps the failure mode uniform across
/// backends: a zero rate or channel count would otherwise surface as
/// anything from an OS error code to a division-by-zero in a backend's
/// period math, depending on which driver won the probe.
fn validate_request(backend: &'static str, req: &StreamRequest) -> Result<()> {
    if req.sample_rate == 0 {
        return Err(Error::UnsupportedFormat {
            backend,
            detail: "requested sample_rate = 0; it must be non-zero".into(),
        });
    }
    if req.channels == 0 {
        return Err(Error::UnsupportedFormat {
            backend,
            detail: "requested channels = 0; at least one output channel is required".into(),
        });
    }
    if req.buffer_frames == Some(0) {
        return Err(Error::UnsupportedFormat {
            backend,
            detail: "requested buffer_frames = Some(0); pass None for the backend default".into(),
        });
    }
    Ok(())
}

/// Open an output stream on the given backend. The callback is invoked
/// from a backend-owned thread with f32 interleaved samples; fill it
/// with audio to play, or write zeros for silence.
///
/// The returned stream starts in the **playing** state — the callback
/// begins running as soon as `open()` returns. Call
/// [`Stream::pause`] first if you need to open ahead of time and start
/// playback later.
///
/// Unsatisfiable requests (`sample_rate == 0`, `channels == 0`,
/// `buffer_frames == Some(0)`) are rejected up front with
/// [`Error::UnsupportedFormat`], uniformly across backends, before any
/// shared library is loaded or device opened.
pub fn open<F>(driver: Driver, req: StreamRequest, cb: F) -> Result<Stream>
where
    F: FnMut(&mut [f32], &CallbackInfo) + Send + 'static,
{
    validate_request(driver.inner.name(), &req)?;
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

/// Open an output stream on a specific enumerated [`Device`]. Equivalent
/// to `open(driver, req.with_device(device.id.clone()), cb)`; supplied
/// as a convenience so the device handle the caller already has from
/// [`Driver::output_devices`] is the natural argument.
///
/// The [`Device`] must come from the same `driver` that's being passed —
/// device ids are backend-native opaque strings and won't resolve on a
/// different backend.
pub fn open_on<F>(driver: Driver, device: &Device, req: StreamRequest, cb: F) -> Result<Stream>
where
    F: FnMut(&mut [f32], &CallbackInfo) + Send + 'static,
{
    open(driver, req.with_device(device.id.clone()), cb)
}

/// Enumerate the playback (output) devices visible on `driver`. Thin
/// wrapper over [`Driver::output_devices`]; see that method for the
/// empty-list-vs-error contract.
pub fn output_devices(driver: Driver) -> Result<Vec<Device>> {
    driver.output_devices()
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

    #[test]
    fn output_devices_never_errors_with_not_implemented() {
        // The public layer maps a backend's `NotImplemented` into an
        // empty list, so iterating every compiled-in driver must never
        // surface that variant — only a genuine runtime failure (which
        // won't happen in CI's headless environment where no library
        // loads) would be an `Err`. We therefore accept either `Ok`
        // (real or empty list) or a non-`NotImplemented` `Err`, but the
        // common headless path is `Ok(vec![])` per backend.
        for d in drivers() {
            match d.output_devices() {
                Ok(_) => {}
                Err(Error::NotImplemented(b)) => {
                    panic!("output_devices leaked NotImplemented for {b}")
                }
                // Library-not-present / device-open failures are fine in
                // a headless CI box without a sound server.
                Err(_) => {}
            }
        }
    }

    #[test]
    fn stream_request_with_device_roundtrip() {
        // The `with_device` builder threads the opaque id into the
        // request without otherwise disturbing rate/channels/format.
        let req = StreamRequest::new(48_000, 2).with_device("plughw:CARD=PCH,DEV=0");
        assert_eq!(req.sample_rate, 48_000);
        assert_eq!(req.channels, 2);
        assert_eq!(req.format, SampleFormat::F32);
        assert_eq!(req.device.as_deref(), Some("plughw:CARD=PCH,DEV=0"));
        // Default ctor leaves `device` unset → backends pick the system
        // default endpoint as before.
        let plain = StreamRequest::new(44_100, 1);
        assert!(plain.device.is_none());
    }

    #[test]
    fn stream_request_with_buffer_frames_builder() {
        // None → backend's own default; Some(N) is a hint, not a constraint.
        let r = StreamRequest::new(48_000, 2).with_buffer_frames(Some(256));
        assert_eq!(r.buffer_frames, Some(256));
        let r2 = r.with_buffer_frames(None);
        assert_eq!(r2.buffer_frames, None);
    }

    #[test]
    fn open_on_with_alien_id_fails_cleanly() {
        // Don't actually open hardware (CI is headless), but exercise
        // the dispatch path: a non-existent id should produce an Err of
        // the *DeviceOpen* / NotImplemented / LibraryLoad family, never
        // a panic, and never an `Ok` on a fabricated id.
        for d in drivers() {
            let dev = Device {
                id: "this-device-does-not-exist-on-any-backend".into(),
                name: "fake".into(),
                is_default: false,
            };
            let req = StreamRequest::new(48_000, 2);
            let r = open_on(d, &dev, req, |_, _| {});
            // We don't assert *which* error variant — that depends on
            // whether the backend's shared library even loads in CI —
            // only that we got one rather than a successful open against
            // a fabricated id.
            assert!(
                r.is_err(),
                "{} accepted a fabricated device id; this is a routing bug",
                d.name()
            );
        }
    }

    #[test]
    fn preferred_format_never_leaks_not_implemented() {
        // Same contract as `output_devices_never_errors_with_not_implemented`:
        // the public layer maps a backend's `NotImplemented` into `Ok(None)`,
        // so iterating every compiled-in driver must never surface that
        // variant. A genuine failure (library present but the introspection
        // call errored) may surface as `Err`. The headless CI path is
        // typically `Ok(None)` (library not loaded → backend's preferred-
        // format returns NotImplemented or DeviceOpen depending on order).
        for d in drivers() {
            match d.preferred_format(None) {
                Ok(_) => {}
                Err(Error::NotImplemented(b)) => {
                    panic!("preferred_format leaked NotImplemented for {b}")
                }
                Err(_) => {}
            }
        }
    }

    #[test]
    fn preferred_format_with_alien_id_does_not_panic() {
        // Same dispatch sanity check as `open_on_with_alien_id_fails_cleanly`:
        // a fabricated id must be reported via the result, never via a
        // panic, and never via a fabricated `Some(StreamFormat)` for a
        // device that doesn't exist. On a headless CI box every backend
        // typically returns `Ok(None)` (library not loaded) or an `Err`
        // (library loaded but the id didn't resolve); both are fine.
        for d in drivers() {
            let alien = Device {
                id: "definitely-not-a-real-device-id-on-any-backend".into(),
                name: "fake".into(),
                is_default: false,
            };
            let _ = d.preferred_format(Some(&alien));
        }
    }

    #[test]
    fn default_output_device_never_leaks_not_implemented() {
        // Mirrors `output_devices_never_errors_with_not_implemented`: the
        // public layer maps a backend's `NotImplemented` into `Ok(None)`,
        // so iterating every compiled-in driver must never surface that
        // variant. A genuine runtime failure (library present but the
        // enumeration call errored) may surface as `Err`. The headless
        // CI path is typically `Ok(None)` (library not loaded → backend's
        // `output_devices()` returns `LibraryLoad`, which the trait
        // default of `default_output_device()` forwards verbatim).
        for d in drivers() {
            match d.default_output_device() {
                Ok(_) => {}
                Err(Error::NotImplemented(b)) => {
                    panic!("default_output_device leaked NotImplemented for {b}")
                }
                Err(_) => {}
            }
        }
    }

    #[test]
    fn default_output_device_matches_enumeration() {
        // The two paths must agree about which entry is the default,
        // because the public-API contract advertises the one-call
        // shortcut as exactly "the entry from `output_devices()` whose
        // `is_default` is set". A divergence is a contract bug.
        //
        // On a headless CI box both paths typically return empty / None,
        // which trivially satisfies the cross-check; the invariant
        // matters on real hardware and is cheap to assert unconditionally
        // here.
        for d in drivers() {
            // Skip drivers that surface a backend error from either
            // side — those paths already have their own coverage and
            // we don't want to conflate "OS refused the call" with
            // "the two accessors disagree".
            let (Ok(default), Ok(list)) = (d.default_output_device(), d.output_devices()) else {
                continue;
            };
            let from_list = list.into_iter().find(|x| x.is_default);
            assert_eq!(
                default,
                from_list,
                "{}: default_output_device() disagrees with the is_default entry from \
                 output_devices()",
                d.name()
            );
        }
    }

    #[test]
    fn default_output_device_carries_is_default_flag() {
        // Whatever a backend returns from `default_output_device()`,
        // a `Some(_)` entry must carry `is_default == true` — otherwise
        // a caller plumbing the returned `Device` into a UI tag would
        // mislabel it. (On a headless CI box every backend typically
        // returns `Ok(None)`, which trivially satisfies this; the
        // invariant matters on a real machine and is cheap to assert
        // unconditionally.)
        for d in drivers() {
            if let Ok(Some(dev)) = d.default_output_device() {
                assert!(
                    dev.is_default,
                    "{}: default_output_device() returned an entry with is_default == false",
                    d.name()
                );
            }
        }
    }

    #[test]
    fn is_stub_marks_only_pipewire_and_asio() {
        // Compile-time stub flag: PipeWire and ASIO are the two backends
        // that ship as placeholders (per README); everything else is
        // functional. The accessor must agree with that contract on
        // whichever target_os this CI runner is — we can't assert on
        // backends that aren't compiled in.
        for d in drivers() {
            let expected = matches!(d.name(), "pipewire" | "asio");
            assert_eq!(
                d.is_stub(),
                expected,
                "{}: is_stub() = {}, expected {}",
                d.name(),
                d.is_stub(),
                expected
            );
        }
    }

    #[test]
    fn is_stub_implies_probe_fails() {
        // The whole point of the stub flag is that the backend is
        // guaranteed-broken at runtime regardless of host configuration.
        // A stub that somehow passes `probe()` would mislead a caller
        // that's using `is_stub()` to skip stubs in a UI.
        for d in drivers() {
            if d.is_stub() {
                let stub_passed_probe = probe().iter().any(|p| std::ptr::eq(p.inner, d.inner));
                assert!(
                    !stub_passed_probe,
                    "{}: is_stub() == true but probe() accepted it",
                    d.name()
                );
            }
        }
    }

    #[test]
    fn is_stub_disjoint_from_probe_results() {
        // Symmetric form of the above: nothing in `probe()`'s output
        // should be a stub. Catches the same bug from the other
        // direction, plus regressions where a future functional backend
        // accidentally inherits the trait default (or vice versa).
        for d in probe() {
            assert!(
                !d.is_stub(),
                "{}: appears in probe() yet is_stub() returns true",
                d.name()
            );
        }
    }

    #[test]
    fn unsatisfiable_requests_rejected_before_dispatch() {
        // Pre-flight validation is backend-independent and runs before
        // any library load, so this is fully deterministic even on a
        // headless CI box: every driver (stubs included) must reject
        // these with UnsupportedFormat, never with a library/probe
        // error, and never by panicking inside period math.
        for d in drivers() {
            let bad = [
                StreamRequest::new(0, 2),
                StreamRequest::new(48_000, 0),
                StreamRequest::new(48_000, 2).with_buffer_frames(Some(0)),
            ];
            for req in bad {
                let desc = format!(
                    "rate={} ch={} buf={:?}",
                    req.sample_rate, req.channels, req.buffer_frames
                );
                match open(d, req, |_, _| {}) {
                    Err(Error::UnsupportedFormat { .. }) => {}
                    Err(e) => panic!("{} [{desc}]: expected UnsupportedFormat, got {e}", d.name()),
                    Ok(_) => panic!("{} [{desc}]: accepted an unsatisfiable request", d.name()),
                }
            }
        }
    }

    #[test]
    fn at_most_one_default_per_driver() {
        // Whatever a backend returns, the list must never claim two
        // defaults. (A headless CI box typically returns an empty list,
        // which trivially satisfies this; the invariant matters on a
        // real machine and is cheap to assert unconditionally.)
        for d in drivers() {
            if let Ok(devs) = d.output_devices() {
                let defaults = devs.iter().filter(|x| x.is_default).count();
                assert!(
                    defaults <= 1,
                    "{} reported {} default devices",
                    d.name(),
                    defaults
                );
            }
        }
    }
}
