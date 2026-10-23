//! ALSA output backend.
//!
//! Loads `libasound.so.2` at runtime through `dlsym` and drives a
//! playback PCM via the `snd_pcm_*` family. Because ALSA's blocking
//! `snd_pcm_writei` doesn't naturally fit a pull-callback model, we
//! spawn a worker thread that loops: invoke the user callback, convert
//! to the device's format if needed, write one period, repeat. Stream
//! drop signals the worker to exit and joins.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use dlsym::{Bindings, Library};

use super::bind_error;
use crate::backend::{Backend, Callback};
use crate::format::{CallbackInfo, Device, SampleFormat, StreamFormat, StreamRequest};
use crate::stream::StreamImpl;
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// ALSA constants
// ---------------------------------------------------------------------------

const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;
const SND_PCM_FORMAT_FLOAT_LE: c_int = 14;
const SND_PCM_FORMAT_S16_LE: c_int = 2;

// snd_pcm_open open-mode flags. 0 = blocking, which is what we want.
const SND_PCM_NONBLOCK: c_int = 0;

// ---------------------------------------------------------------------------
// Opaque types — we only ever pass these through as pointers.
// ---------------------------------------------------------------------------

#[repr(C)]
struct snd_pcm_t {
    _private: [u8; 0],
}
#[repr(C)]
struct snd_pcm_hw_params_t {
    _private: [u8; 0],
}

dlsym::library! {
    /// `libasound.so.2`, with ALSA's spelling for the required PCM /
    /// hw_params surface. The name-hint trio and `set_channels_near`
    /// are `#[optional]`: a libasound old enough to lack them still
    /// plays audio, it just can't enumerate (or hint a channel count).
    struct Asound {
        unsafe extern "C" {
            fn snd_pcm_open(
                pcmp: *mut *mut snd_pcm_t,
                name: *const c_char,
                stream: c_int,
                mode: c_int,
            ) -> c_int;
            fn snd_pcm_close(pcm: *mut snd_pcm_t) -> c_int;
            fn snd_pcm_prepare(pcm: *mut snd_pcm_t) -> c_int;
            fn snd_pcm_writei(pcm: *mut snd_pcm_t, buffer: *const c_void, size: c_ulong) -> c_long;
            fn snd_pcm_recover(pcm: *mut snd_pcm_t, err: c_int, silent: c_int) -> c_int;
            fn snd_pcm_drop(pcm: *mut snd_pcm_t) -> c_int;
            fn snd_pcm_pause(pcm: *mut snd_pcm_t, enable: c_int) -> c_int;
            fn snd_pcm_delay(pcm: *mut snd_pcm_t, delayp: *mut c_long) -> c_int;
            fn snd_strerror(errnum: c_int) -> *const c_char;

            // Device-enumeration ("name hint") API. `snd_device_name_hint` fills a
            // NULL-terminated array of opaque hint pointers; each pointer answers
            // `snd_device_name_get_hint(hint, key)` for keys "NAME" / "DESC" /
            // "IOID" with a freshly malloc'd C string (caller frees with the libc
            // `free`, which we reach via `snd_device_name_free_hint` for the array
            // and a stored libc `free` for the individual strings — but ALSA docs
            // say the per-string buffers are also freed by `free`; we keep them
            // short-lived and free each immediately). The whole array is released
            // by `snd_device_name_free_hint`. Present on every libasound since
            // 1.0.14 (2007) but resolved defensively so a missing one degrades
            // enumeration to empty rather than failing the whole load.
            #[optional]
            fn snd_device_name_hint(
                card: c_int,
                iface: *const c_char,
                hints: *mut *mut *mut c_void,
            ) -> c_int;
            #[optional]
            fn snd_device_name_get_hint(hint: *const c_void, id: *const c_char) -> *mut c_char;
            #[optional]
            fn snd_device_name_free_hint(hints: *mut *mut c_void) -> c_int;

            #[link_name = "snd_pcm_hw_params_malloc"]
            fn hw_params_malloc(ptr: *mut *mut snd_pcm_hw_params_t) -> c_int;
            #[link_name = "snd_pcm_hw_params_free"]
            fn hw_params_free(obj: *mut snd_pcm_hw_params_t);
            #[link_name = "snd_pcm_hw_params_any"]
            fn hw_params_any(pcm: *mut snd_pcm_t, params: *mut snd_pcm_hw_params_t) -> c_int;
            #[link_name = "snd_pcm_hw_params_set_access"]
            fn hw_params_set_access(
                pcm: *mut snd_pcm_t,
                params: *mut snd_pcm_hw_params_t,
                access: c_int,
            ) -> c_int;
            #[link_name = "snd_pcm_hw_params_set_format"]
            fn hw_params_set_format(
                pcm: *mut snd_pcm_t,
                params: *mut snd_pcm_hw_params_t,
                format: c_int,
            ) -> c_int;
            #[link_name = "snd_pcm_hw_params_set_channels"]
            fn hw_params_set_channels(
                pcm: *mut snd_pcm_t,
                params: *mut snd_pcm_hw_params_t,
                val: c_uint,
            ) -> c_int;
            /// Channel-count negotiation analogue of `set_rate_near` — clamps
            /// the requested count into the device's supported range and writes
            /// the snapped value back through the mutable arg. Optional so a
            /// libasound old enough to lack it just degrades `preferred_format`
            /// to "no channel-count hint" rather than failing the whole load.
            #[optional]
            #[link_name = "snd_pcm_hw_params_set_channels_near"]
            fn hw_params_set_channels_near(
                pcm: *mut snd_pcm_t,
                params: *mut snd_pcm_hw_params_t,
                val: *mut c_uint,
            ) -> c_int;
            #[link_name = "snd_pcm_hw_params_set_rate_near"]
            fn hw_params_set_rate_near(
                pcm: *mut snd_pcm_t,
                params: *mut snd_pcm_hw_params_t,
                val: *mut c_uint,
                dir: *mut c_int,
            ) -> c_int;
            #[link_name = "snd_pcm_hw_params_set_period_size_near"]
            fn hw_params_set_period_size_near(
                pcm: *mut snd_pcm_t,
                params: *mut snd_pcm_hw_params_t,
                val: *mut c_ulong,
                dir: *mut c_int,
            ) -> c_int;
            #[link_name = "snd_pcm_hw_params_set_buffer_size_near"]
            fn hw_params_set_buffer_size_near(
                pcm: *mut snd_pcm_t,
                params: *mut snd_pcm_hw_params_t,
                val: *mut c_ulong,
            ) -> c_int;
            #[link_name = "snd_pcm_hw_params_get_period_size"]
            fn hw_params_get_period_size(
                params: *const snd_pcm_hw_params_t,
                val: *mut c_ulong,
                dir: *mut c_int,
            ) -> c_int;
            #[link_name = "snd_pcm_hw_params"]
            fn hw_params(pcm: *mut snd_pcm_t, params: *mut snd_pcm_hw_params_t) -> c_int;
        }
    }
}

/// The required symbols `Asound` binds (as libasound exports them), for
/// [`bind_error`]'s `SymbolMissing` naming.
const ASOUND_SYMBOLS: &[&str] = &[
    "snd_pcm_open",
    "snd_pcm_close",
    "snd_pcm_prepare",
    "snd_pcm_writei",
    "snd_pcm_recover",
    "snd_pcm_drop",
    "snd_pcm_pause",
    "snd_pcm_delay",
    "snd_strerror",
    "snd_pcm_hw_params_malloc",
    "snd_pcm_hw_params_free",
    "snd_pcm_hw_params_any",
    "snd_pcm_hw_params_set_access",
    "snd_pcm_hw_params_set_format",
    "snd_pcm_hw_params_set_channels",
    "snd_pcm_hw_params_set_rate_near",
    "snd_pcm_hw_params_set_period_size_near",
    "snd_pcm_hw_params_set_buffer_size_near",
    "snd_pcm_hw_params_get_period_size",
    "snd_pcm_hw_params",
];

dlsym::library! {
    /// libc's `free`, used to release the per-hint strings ALSA hands back.
    struct LibcFree {
        unsafe extern "C" {
            fn free(p: *mut c_void);
        }
    }
}

// ---------------------------------------------------------------------------
// Loaded library + resolved symbols.
// ---------------------------------------------------------------------------

struct AlsaLib {
    asound: Asound,
    /// libc `free` for the malloc'd hint strings. `None` means
    /// enumeration leaks the per-hint strings (bounded — a one-shot
    /// enumeration), so the load never fails on its account.
    libc: Option<LibcFree>,
}

impl AlsaLib {
    fn load_default() -> Result<Arc<Self>> {
        const SONAME: &str = "libasound.so.2";
        // SAFETY: libasound's initialisers are trusted.
        let lib = unsafe { Library::open(SONAME) }.map_err(|e| Error::LibraryLoad {
            backend: "alsa",
            soname: SONAME,
            source: e,
        })?;
        let asound = <Asound as Bindings>::load(lib)
            .map_err(|e| bind_error("alsa", SONAME, ASOUND_SYMBOLS, e))?;
        Ok(Arc::new(AlsaLib {
            asound,
            libc: load_libc_free(),
        }))
    }

    fn strerror(&self, err: c_int) -> String {
        unsafe {
            let p = self.asound.snd_strerror(err);
            if p.is_null() {
                format!("alsa error {err}")
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    /// Read one hint key (`"NAME"` / `"DESC"` / `"IOID"`) off a hint
    /// pointer, copying it into an owned `String` and freeing the
    /// libasound-malloc'd buffer. Returns `None` when the key is absent
    /// (ALSA returns NULL — e.g. IOID is NULL for a duplex device).
    unsafe fn get_hint(&self, hint: *const c_void, key: &CStr) -> Option<String> {
        unsafe {
            let raw = self.asound.snd_device_name_get_hint(hint, key.as_ptr())?;
            if raw.is_null() {
                return None;
            }
            let owned = CStr::from_ptr(raw).to_string_lossy().into_owned();
            if let Some(libc) = &self.libc {
                libc.free(raw as *mut c_void);
            }
            Some(owned)
        }
    }

    /// Enumerate playback PCM devices via the `snd_device_name_hint`
    /// API. ALSA returns a NULL-terminated array of opaque hints, each
    /// carrying NAME (the PCM string we pass to `snd_pcm_open`), DESC
    /// (the friendly multi-line description) and IOID ("Input" /
    /// "Output" / NULL=both). We keep hints whose IOID is Output or
    /// unset, and tag the one named `"default"` as the system default.
    fn enumerate(&self) -> Result<Vec<Device>> {
        let iface = CString::new("pcm").unwrap();
        let key_name = CString::new("NAME").unwrap();
        let key_desc = CString::new("DESC").unwrap();
        let key_ioid = CString::new("IOID").unwrap();

        let mut out = Vec::new();
        unsafe {
            let mut hints: *mut *mut c_void = ptr::null_mut();
            // card = -1 means "all cards". A libasound without the
            // name-hint API (pre-1.0.14) cannot enumerate at all.
            let r = self
                .asound
                .snd_device_name_hint(-1, iface.as_ptr(), &mut hints)
                .ok_or(Error::NotImplemented("alsa"))?;
            if r < 0 {
                return Err(Error::Runtime {
                    backend: "alsa",
                    detail: format!("snd_device_name_hint: {}", self.strerror(r)),
                });
            }
            if hints.is_null() {
                return Ok(out);
            }
            let mut p = hints;
            while !(*p).is_null() {
                let hint = *p as *const c_void;
                // Output filter: keep "Output" and NULL (duplex); drop
                // "Input"-only capture endpoints, which would otherwise
                // appear in this playback-oriented list.
                let ioid = self.get_hint(hint, &key_ioid);
                let is_output = match ioid.as_deref() {
                    Some("Output") | None => true,
                    Some(_) => false,
                };
                if is_output && let Some(name) = self.get_hint(hint, &key_name) {
                    // DESC's first line is the friendly card label;
                    // subsequent lines are the long form. Take the
                    // first line, fall back to NAME if DESC absent.
                    let desc = self.get_hint(hint, &key_desc);
                    let friendly = desc
                        .as_deref()
                        .and_then(|d| d.lines().next())
                        .unwrap_or(&name)
                        .to_string();
                    let is_default = name == "default";
                    out.push(Device {
                        id: name,
                        name: friendly,
                        is_default,
                    });
                }
                p = p.add(1);
            }
            // `free_hint` ships together with `name_hint`; should a
            // libasound somehow lack only it, the one-shot hint array
            // leaks rather than the enumeration failing.
            let _ = self.asound.snd_device_name_free_hint(hints);
        }
        Ok(out)
    }
}

/// Resolve libc's `free` into its own owning binding (kept alive in
/// `AlsaLib`). Best-effort: a `None` result just means the name-hint
/// strings leak per enumeration call, which is bounded and never
/// breaks playback.
fn load_libc_free() -> Option<LibcFree> {
    // Glibc, musl, and the BSD-flavoured libc on some distros all expose
    // `free` from one of these sonames. We deliberately do not depend on
    // the `libc` crate — staying header- and link-free is this crate's
    // whole premise.
    const CANDIDATES: &[&str] = &["libc.so.6", "libc.so", "libc.musl-x86_64.so.1"];
    // SAFETY: libc is already loaded in this process; re-opening it
    // runs no initialiser we do not already trust.
    unsafe { LibcFree::open_first(CANDIDATES) }.ok()
}

// Cached so we don't dlopen once per stream.
fn lib() -> Result<Arc<AlsaLib>> {
    static CACHED: OnceLock<Mutex<Option<Arc<AlsaLib>>>> = OnceLock::new();
    let slot = CACHED.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap();
    if let Some(l) = guard.as_ref() {
        return Ok(l.clone());
    }
    let l = AlsaLib::load_default()?;
    *guard = Some(l.clone());
    Ok(l)
}

// ---------------------------------------------------------------------------
// Backend impl.
// ---------------------------------------------------------------------------

pub(crate) struct AlsaBackend;

impl Backend for AlsaBackend {
    fn name(&self) -> &'static str {
        "alsa"
    }
    fn description(&self) -> &'static str {
        "ALSA (libasound.so.2 via dlsym)"
    }

    fn probe(&self) -> Result<()> {
        let l = lib()?;
        unsafe {
            let mut pcm: *mut snd_pcm_t = ptr::null_mut();
            let name = CString::new("default").unwrap();
            let r = l.asound.snd_pcm_open(
                &mut pcm,
                name.as_ptr(),
                SND_PCM_STREAM_PLAYBACK,
                SND_PCM_NONBLOCK,
            );
            if r < 0 {
                return Err(Error::DeviceOpen {
                    backend: "alsa",
                    detail: l.strerror(r),
                });
            }
            l.asound.snd_pcm_close(pcm);
        }
        Ok(())
    }

    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>> {
        let l = lib()?;
        unsafe { open_inner(l, req, cb) }
    }

    fn output_devices(&self) -> Result<Vec<Device>> {
        lib()?.enumerate()
    }

    /// Throwaway-open + `hw_params_any` to read what the device would
    /// pick for an unconstrained request. ALSA's hw_params negotiation
    /// is in-band so this is the only way to surface the device's
    /// preferred rate / channel count without holding a stream open.
    /// We feed a canonical 48 kHz / 2 ch hint into the `_near` helpers
    /// and read the snapped values back out of their mutable args —
    /// exactly the path `open()` uses to land on the actual stream
    /// rate, just without committing the params or starting the
    /// worker. The PCM is closed before we return so the caller can
    /// follow up with a real `open()` against the same device without
    /// contention.
    fn preferred_format(&self, device_id: Option<&str>) -> Result<StreamFormat> {
        let l = lib()?;
        let pcm_name = device_id.unwrap_or("default");
        let name = CString::new(pcm_name).map_err(|_| Error::DeviceOpen {
            backend: "alsa",
            detail: "device id contains an interior NUL byte".into(),
        })?;
        unsafe {
            let mut pcm: *mut snd_pcm_t = ptr::null_mut();
            // NONBLOCK so a busy device doesn't make `preferred_format`
            // wait for it — we just want to read the param-space
            // metadata, not drive samples.
            let r = l.asound.snd_pcm_open(
                &mut pcm,
                name.as_ptr(),
                SND_PCM_STREAM_PLAYBACK,
                SND_PCM_NONBLOCK,
            );
            if r < 0 {
                return Err(Error::DeviceOpen {
                    backend: "alsa",
                    detail: l.strerror(r),
                });
            }
            // From here on we must close `pcm` on every exit path.
            let res = preferred_format_inner(&l, pcm);
            l.asound.snd_pcm_close(pcm);
            res
        }
    }
}

/// Inner helper for `preferred_format`. Assumes `pcm` is an open PCM
/// handle; the caller is responsible for closing it. Uses a local RAII
/// guard for the hw_params allocation so every error path releases it
/// before unwinding to `preferred_format`'s `snd_pcm_close`.
unsafe fn preferred_format_inner(l: &AlsaLib, pcm: *mut snd_pcm_t) -> Result<StreamFormat> {
    unsafe {
        struct LocalHwGuard<'a> {
            l: &'a AlsaLib,
            hw: *mut snd_pcm_hw_params_t,
        }
        impl<'a> Drop for LocalHwGuard<'a> {
            fn drop(&mut self) {
                unsafe { self.l.asound.hw_params_free(self.hw) };
            }
        }

        let mut hw: *mut snd_pcm_hw_params_t = ptr::null_mut();
        let r = l.asound.hw_params_malloc(&mut hw);
        if r < 0 {
            return Err(Error::DeviceOpen {
                backend: "alsa",
                detail: format!("hw_params_malloc: {}", l.strerror(r)),
            });
        }
        let _guard = LocalHwGuard { l, hw };

        // Load the full parameter superset for the device.
        check(l, l.asound.hw_params_any(pcm, hw), "hw_params_any")?;
        // Sample format — mirror the open path's preference (FLOAT_LE, fall
        // back to S16_LE). The public callback surface stays F32 either way;
        // the open path silently converts S16<->F32 for old devices. So the
        // *reported* format here is always F32 — what we're really probing
        // for is whether the device will accept the open path at all.
        let format = if l
            .asound
            .hw_params_set_format(pcm, hw, SND_PCM_FORMAT_FLOAT_LE)
            == 0
        {
            SampleFormat::F32
        } else {
            check(
                l,
                l.asound
                    .hw_params_set_format(pcm, hw, SND_PCM_FORMAT_S16_LE),
                "hw_params_set_format(S16_LE)",
            )?;
            SampleFormat::F32
        };
        // Channel-count snap — canonical 2 (stereo) hint, let ALSA pull it
        // into the device's nearest supported count. The symbol is
        // optional; if libasound is old enough to lack it we keep 2 as the
        // documented default. Stereo is the common reality and matches the
        // backend's other clamping (`channels.clamp(1, 8)` on `open`).
        let mut channels: c_uint = 2;
        if let Some(r) = l.asound.hw_params_set_channels_near(pcm, hw, &mut channels) {
            check(l, r, "hw_params_set_channels_near")?;
        }
        // Sample-rate snap — canonical 48 kHz hint, exactly the path the
        // open() comment in lib.rs documents:
        // "the rate `snd_pcm_hw_params_set_rate_near` snaps the
        //  unconstrained request to".
        let mut rate: c_uint = 48_000;
        let mut dir: c_int = 0;
        check(
            l,
            l.asound
                .hw_params_set_rate_near(pcm, hw, &mut rate, &mut dir),
            "hw_params_set_rate_near",
        )?;
        Ok(StreamFormat {
            sample_rate: rate,
            channels: channels.clamp(1, u16::MAX as c_uint) as u16,
            format,
        })
    }
}

unsafe fn open_inner(
    l: Arc<AlsaLib>,
    req: StreamRequest,
    cb: Callback,
) -> Result<Box<dyn StreamImpl>> {
    unsafe {
        // Honor `req.device` if the caller wants a specific PCM. For ALSA
        // the enumerated `Device::id` IS the PCM name (`snd_device_name_hint`
        // returns it under the "NAME" key), so we pass it straight to
        // `snd_pcm_open`. A NUL byte inside the string would be a malformed
        // id; reject it as DeviceOpen rather than silently truncating.
        let pcm_name = req.device.as_deref().unwrap_or("default");
        let name = CString::new(pcm_name).map_err(|_| Error::DeviceOpen {
            backend: "alsa",
            detail: "device id contains an interior NUL byte".into(),
        })?;
        let mut pcm: *mut snd_pcm_t = ptr::null_mut();
        let r = l
            .asound
            .snd_pcm_open(&mut pcm, name.as_ptr(), SND_PCM_STREAM_PLAYBACK, 0);
        if r < 0 {
            return Err(Error::DeviceOpen {
                backend: "alsa",
                detail: l.strerror(r),
            });
        }
        // From here on, any error path must close pcm.
        let res = configure_and_spawn(l.clone(), pcm, req, cb);
        match res {
            Ok(boxed) => Ok(boxed),
            Err(e) => {
                l.asound.snd_pcm_close(pcm);
                Err(e)
            }
        }
    }
}

unsafe fn configure_and_spawn(
    l: Arc<AlsaLib>,
    pcm: *mut snd_pcm_t,
    req: StreamRequest,
    cb: Callback,
) -> Result<Box<dyn StreamImpl>> {
    unsafe {
        let mut hw: *mut snd_pcm_hw_params_t = ptr::null_mut();
        let r = l.asound.hw_params_malloc(&mut hw);
        if r < 0 {
            return Err(Error::DeviceOpen {
                backend: "alsa",
                detail: format!("hw_params_malloc: {}", l.strerror(r)),
            });
        }
        let hw_guard = HwParamsGuard { l: l.clone(), hw };

        check(&l, l.asound.hw_params_any(pcm, hw), "hw_params_any")?;
        check(
            &l,
            l.asound
                .hw_params_set_access(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED),
            "hw_params_set_access",
        )?;

        // Prefer FLOAT_LE; fall back to S16_LE for devices that don't
        // advertise float (uncommon on modern drivers, common on embedded).
        let (device_fmt, device_fmt_is_f32) =
            match l
                .asound
                .hw_params_set_format(pcm, hw, SND_PCM_FORMAT_FLOAT_LE)
            {
                0 => (SND_PCM_FORMAT_FLOAT_LE, true),
                _ => {
                    check(
                        &l,
                        l.asound
                            .hw_params_set_format(pcm, hw, SND_PCM_FORMAT_S16_LE),
                        "hw_params_set_format(S16_LE)",
                    )?;
                    (SND_PCM_FORMAT_S16_LE, false)
                }
            };
        let _ = device_fmt;

        let want_ch = req.channels.clamp(1, 8) as c_uint;
        check(
            &l,
            l.asound.hw_params_set_channels(pcm, hw, want_ch),
            "hw_params_set_channels",
        )?;

        let mut rate = req.sample_rate;
        let mut dir: c_int = 0;
        check(
            &l,
            l.asound
                .hw_params_set_rate_near(pcm, hw, &mut rate, &mut dir),
            "hw_params_set_rate_near",
        )?;

        // Target ~20 ms periods; the caller can ask for something else via
        // StreamRequest::buffer_frames. 4 periods of headroom keeps us out
        // of underrun range on loaded systems.
        let mut period: c_ulong = req
            .buffer_frames
            .map(|b| b as c_ulong)
            .unwrap_or_else(|| (rate as c_ulong / 50).max(64));
        let mut dir2: c_int = 0;
        check(
            &l,
            l.asound
                .hw_params_set_period_size_near(pcm, hw, &mut period, &mut dir2),
            "hw_params_set_period_size_near",
        )?;

        let mut buffer: c_ulong = period.saturating_mul(4);
        let _ = l
            .asound
            .hw_params_set_buffer_size_near(pcm, hw, &mut buffer); // best-effort

        check(&l, l.asound.hw_params(pcm, hw), "hw_params (commit)")?;

        let mut period_actual: c_ulong = 0;
        let mut pd: c_int = 0;
        let _ = l
            .asound
            .hw_params_get_period_size(hw, &mut period_actual, &mut pd);
        drop(hw_guard);

        check(&l, l.asound.snd_pcm_prepare(pcm), "snd_pcm_prepare")?;

        let period_frames = period_actual.max(period) as usize;
        let channels = want_ch as usize;
        let frames_played = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        // Latest `snd_pcm_delay()` reading, in frames. Negative values are
        // ALSA's "the PCM is ahead of us" which shouldn't happen for
        // playback; we store i64 to match the sign of snd_pcm_sframes_t.
        let delay_frames = Arc::new(AtomicI64::new(0));

        let state = AlsaWorkerState {
            lib: l.clone(),
            pcm: PcmPtr(pcm),
            cb,
            period_frames,
            channels,
            device_fmt_is_f32,
            frames_played: frames_played.clone(),
            stop: stop.clone(),
            delay_frames: delay_frames.clone(),
        };

        let thread = std::thread::Builder::new()
            .name("oxideav-sysaudio-alsa".into())
            .spawn(move || state.run())
            .map_err(|e| Error::Runtime {
                backend: "alsa",
                detail: format!("spawn worker: {e}"),
            })?;

        Ok(Box::new(AlsaStream {
            lib: l,
            pcm: PcmPtr(pcm),
            stop,
            thread: Some(thread),
            delay_frames,
            format: StreamFormat {
                sample_rate: rate,
                channels: want_ch as u16,
                format: SampleFormat::F32,
            },
        }))
    }
}

fn check(l: &AlsaLib, r: c_int, ctx: &'static str) -> Result<()> {
    if r < 0 {
        Err(Error::DeviceOpen {
            backend: "alsa",
            detail: format!("{ctx}: {}", l.strerror(r)),
        })
    } else {
        Ok(())
    }
}

struct HwParamsGuard {
    l: Arc<AlsaLib>,
    hw: *mut snd_pcm_hw_params_t,
}
impl Drop for HwParamsGuard {
    fn drop(&mut self) {
        unsafe { self.l.asound.hw_params_free(self.hw) }
    }
}

// Newtype so raw pointer crosses thread boundaries. ALSA PCM handles
// are not intrinsically Send/Sync in libasound's contract; in practice
// only the worker thread touches the handle while the stream is live.
#[derive(Copy, Clone)]
struct PcmPtr(*mut snd_pcm_t);
unsafe impl Send for PcmPtr {}

struct AlsaWorkerState {
    lib: Arc<AlsaLib>,
    pcm: PcmPtr,
    cb: Callback,
    period_frames: usize,
    channels: usize,
    device_fmt_is_f32: bool,
    frames_played: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    /// Published each iteration after `snd_pcm_writei` returns, read
    /// by `Stream::latency()` on another thread.
    delay_frames: Arc<AtomicI64>,
}

impl AlsaWorkerState {
    fn run(mut self) {
        let mut f32_buf = vec![0.0f32; self.period_frames * self.channels];
        // When the device is S16_LE we keep a scratch for the converted
        // samples so we can pass a contiguous byte pointer to writei.
        let mut s16_buf: Vec<i16> = if self.device_fmt_is_f32 {
            Vec::new()
        } else {
            vec![0; self.period_frames * self.channels]
        };

        while !self.stop.load(Ordering::Relaxed) {
            let info = CallbackInfo {
                frames_played: self.frames_played.load(Ordering::Relaxed),
            };
            (self.cb)(&mut f32_buf, &info);

            let r = unsafe {
                if self.device_fmt_is_f32 {
                    self.lib.asound.snd_pcm_writei(
                        self.pcm.0,
                        f32_buf.as_ptr() as *const c_void,
                        self.period_frames as c_ulong,
                    )
                } else {
                    for (d, &s) in s16_buf.iter_mut().zip(f32_buf.iter()) {
                        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i32;
                        *d = v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    }
                    self.lib.asound.snd_pcm_writei(
                        self.pcm.0,
                        s16_buf.as_ptr() as *const c_void,
                        self.period_frames as c_ulong,
                    )
                }
            };
            if r < 0 {
                // -EPIPE / -ESTRPIPE / -EBADFD → ask ALSA to recover.
                let rec = unsafe { self.lib.asound.snd_pcm_recover(self.pcm.0, r as c_int, 1) };
                if rec < 0 {
                    // Unrecoverable — bail.
                    return;
                }
                continue;
            }
            self.frames_played.fetch_add(r as u64, Ordering::Relaxed);

            // Publish driver-side queue depth for `Stream::latency()`.
            // Safe to call from this thread only (snd_pcm handles are
            // not thread-safe); readers get a stale-by-one-period view.
            let mut delay: c_long = 0;
            let dr = unsafe { self.lib.asound.snd_pcm_delay(self.pcm.0, &mut delay) };
            if dr == 0 {
                self.delay_frames.store(delay as i64, Ordering::Relaxed);
            }
        }
    }
}

struct AlsaStream {
    lib: Arc<AlsaLib>,
    pcm: PcmPtr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Latest snd_pcm_delay() reading, published by the worker.
    delay_frames: Arc<AtomicI64>,
    format: StreamFormat,
}

// PCM handle only touched by the worker thread while running; on stop
// the main thread closes it after joining, so cross-thread moves are
// fine.
unsafe impl Send for AlsaStream {}

impl StreamImpl for AlsaStream {
    fn play(&mut self) -> Result<()> {
        unsafe {
            // `snd_pcm_pause(pcm, 0)` un-pauses. Many software devices
            // return -ENOTSUP for pause; ignore that — on those devices
            // the stream is always "playing" and we simulated pause
            // by writing silence from the callback instead.
            let _ = self.lib.asound.snd_pcm_pause(self.pcm.0, 0);
        }
        Ok(())
    }
    fn pause(&mut self) -> Result<()> {
        unsafe {
            let _ = self.lib.asound.snd_pcm_pause(self.pcm.0, 1);
        }
        Ok(())
    }
    fn format(&self) -> StreamFormat {
        self.format
    }
    fn latency(&self) -> Option<Duration> {
        let frames = self.delay_frames.load(Ordering::Relaxed).max(0) as u64;
        let rate = self.format.sample_rate.max(1) as u64;
        let nanos = frames.saturating_mul(1_000_000_000) / rate;
        Some(Duration::from_nanos(nanos))
    }
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            let _ = self.lib.asound.snd_pcm_drop(self.pcm.0);
            let _ = self.lib.asound.snd_pcm_close(self.pcm.0);
        }
    }
}
