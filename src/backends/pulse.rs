//! PulseAudio output backend via the "simple" API.
//!
//! `libpulse-simple.so.0` hides the PulseAudio main loop and gives us a
//! blocking `pa_simple_write`; we run that in a worker thread exactly
//! like the ALSA backend so the user-facing callback surface is
//! identical.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use libloading::{Library, Symbol};

use crate::backend::{Backend, Callback};
use crate::format::{CallbackInfo, SampleFormat, StreamFormat, StreamRequest};
use crate::stream::StreamImpl;
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// PulseAudio constants and ABI types.
// ---------------------------------------------------------------------------

/// `pa_stream_direction_t::PA_STREAM_PLAYBACK`.
const PA_STREAM_PLAYBACK: c_int = 1;
/// `pa_sample_format_t::PA_SAMPLE_FLOAT32LE`. Value comes from
/// pulseaudio's sample.h; confirmed stable across 14.x and 17.x.
const PA_SAMPLE_FLOAT32LE: c_int = 5;

#[repr(C)]
struct pa_simple {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct pa_sample_spec {
    format: c_int,
    rate: u32,
    channels: u8,
}

/// `pa_buffer_attr` from `pulse/def.h`. Five `u32` fields ordered
/// `maxlength`, `tlength`, `prebuf`, `minreq`, `fragsize`. Each is a
/// **byte** count or `u32::MAX` ("let the server pick"). For playback
/// the server reads `tlength` (target playback-buffer length) and
/// `minreq` (smallest poll the server wants before refilling); the
/// other three fields belong to the capture path or to overall sizing
/// and stay sentinel.
#[repr(C)]
#[derive(Clone, Copy)]
struct pa_buffer_attr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type Fn_pa_simple_new = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const pa_sample_spec,
    map: *const c_void,
    attr: *const pa_buffer_attr,
    error: *mut c_int,
) -> *mut pa_simple;

type Fn_pa_simple_free = unsafe extern "C" fn(s: *mut pa_simple);
type Fn_pa_simple_write = unsafe extern "C" fn(
    s: *mut pa_simple,
    data: *const c_void,
    bytes: usize,
    error: *mut c_int,
) -> c_int;
type Fn_pa_simple_drain = unsafe extern "C" fn(s: *mut pa_simple, error: *mut c_int) -> c_int;
type Fn_pa_simple_flush = unsafe extern "C" fn(s: *mut pa_simple, error: *mut c_int) -> c_int;
/// Returns latency in microseconds (`pa_usec_t = u64`), or `(pa_usec_t)-1`
/// via `error` on failure.
type Fn_pa_simple_get_latency = unsafe extern "C" fn(s: *mut pa_simple, error: *mut c_int) -> u64;
type Fn_pa_strerror = unsafe extern "C" fn(error: c_int) -> *const c_char;

struct PulseLib {
    _lib: Library,
    pa_simple_new: Fn_pa_simple_new,
    pa_simple_free: Fn_pa_simple_free,
    pa_simple_write: Fn_pa_simple_write,
    /// Resolved-but-unused today; kept around because closing out
    /// gracefully is going to want it once we add a `drain()` surface.
    #[allow(dead_code)]
    pa_simple_drain: Fn_pa_simple_drain,
    pa_simple_flush: Fn_pa_simple_flush,
    pa_simple_get_latency: Fn_pa_simple_get_latency,
    pa_strerror: Fn_pa_strerror,
}

unsafe impl Send for PulseLib {}
unsafe impl Sync for PulseLib {}

impl PulseLib {
    fn load() -> Result<Arc<Self>> {
        unsafe {
            let lib = Library::new("libpulse-simple.so.0").map_err(|e| Error::LibraryLoad {
                backend: "pulse",
                soname: "libpulse-simple.so.0",
                source: e,
            })?;
            macro_rules! sym {
                ($name:ident, $ty:ty) => {{
                    let s: Symbol<$ty> = lib
                        .get(concat!(stringify!($name), "\0").as_bytes())
                        .map_err(|e| Error::SymbolMissing {
                            backend: "pulse",
                            symbol: stringify!($name),
                            source: e,
                        })?;
                    *s
                }};
            }
            Ok(Arc::new(PulseLib {
                pa_simple_new: sym!(pa_simple_new, Fn_pa_simple_new),
                pa_simple_free: sym!(pa_simple_free, Fn_pa_simple_free),
                pa_simple_write: sym!(pa_simple_write, Fn_pa_simple_write),
                pa_simple_drain: sym!(pa_simple_drain, Fn_pa_simple_drain),
                pa_simple_flush: sym!(pa_simple_flush, Fn_pa_simple_flush),
                pa_simple_get_latency: sym!(pa_simple_get_latency, Fn_pa_simple_get_latency),
                pa_strerror: sym!(pa_strerror, Fn_pa_strerror),
                _lib: lib,
            }))
        }
    }

    fn strerror(&self, err: c_int) -> String {
        unsafe {
            let p = (self.pa_strerror)(err);
            if p.is_null() {
                format!("pa error {err}")
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

fn lib() -> Result<Arc<PulseLib>> {
    static CACHED: OnceLock<Mutex<Option<Arc<PulseLib>>>> = OnceLock::new();
    let slot = CACHED.get_or_init(|| Mutex::new(None));
    let mut g = slot.lock().unwrap();
    if let Some(l) = g.as_ref() {
        return Ok(l.clone());
    }
    let l = PulseLib::load()?;
    *g = Some(l.clone());
    Ok(l)
}

// ---------------------------------------------------------------------------
// Backend impl.
// ---------------------------------------------------------------------------

/// Build a `pa_buffer_attr` from a frame-count hint, the stream's
/// sample rate, and the per-frame byte size. `tlength` is the requested
/// playback-buffer byte count; `minreq` is the smallest refill the
/// server should ask for and is capped at `tlength` so the server never
/// rejects a hint with `minreq > tlength`. The other three fields stay
/// sentinel (`u32::MAX`) so the server picks defaults for them, the
/// same as what NULL would have done.
fn make_buffer_attr(
    buffer_frames: u32,
    sample_rate: u32,
    bytes_per_frame: usize,
) -> pa_buffer_attr {
    let tlength_bytes = (buffer_frames as u64).saturating_mul(bytes_per_frame as u64);
    let tlength = u32::try_from(tlength_bytes).unwrap_or(u32::MAX);
    // ~20 ms of frames at the request's rate, but never above the
    // tlength itself (the server rejects `minreq > tlength`).
    let one_period_frames = ((sample_rate as usize) / 50).max(64);
    let one_period_bytes = (one_period_frames as u64).saturating_mul(bytes_per_frame as u64);
    let minreq_bytes = one_period_bytes.min(tlength_bytes.max(1));
    let minreq = u32::try_from(minreq_bytes).unwrap_or(u32::MAX);
    pa_buffer_attr {
        maxlength: u32::MAX,
        tlength,
        prebuf: u32::MAX,
        minreq,
        fragsize: u32::MAX,
    }
}

pub(crate) struct PulseBackend;

impl Backend for PulseBackend {
    fn name(&self) -> &'static str {
        "pulse"
    }
    fn description(&self) -> &'static str {
        "PulseAudio (libpulse-simple.so.0 via libloading)"
    }
    fn probe(&self) -> Result<()> {
        let l = lib()?;
        let spec = pa_sample_spec {
            format: PA_SAMPLE_FLOAT32LE,
            rate: 44_100,
            channels: 2,
        };
        let app = CString::new("oxideav-sysaudio").unwrap();
        let name = CString::new("probe").unwrap();
        unsafe {
            let mut err: c_int = 0;
            let s = (l.pa_simple_new)(
                ptr::null(),
                app.as_ptr(),
                PA_STREAM_PLAYBACK,
                ptr::null(),
                name.as_ptr(),
                &spec,
                ptr::null(),
                ptr::null::<pa_buffer_attr>(),
                &mut err,
            );
            if s.is_null() {
                return Err(Error::DeviceOpen {
                    backend: "pulse",
                    detail: l.strerror(err),
                });
            }
            (l.pa_simple_free)(s);
        }
        Ok(())
    }

    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>> {
        let l = lib()?;
        let channels = req.channels.clamp(1, 8);
        let spec = pa_sample_spec {
            format: PA_SAMPLE_FLOAT32LE,
            rate: req.sample_rate,
            channels: channels as u8,
        };
        let app = CString::new("oxideav-sysaudio").unwrap();
        let stream_name = CString::new("playback").unwrap();
        // If the caller named a specific sink, hand it to `pa_simple_new`
        // as `dev`. The "simple" API doesn't enumerate sinks itself, but
        // the underlying PulseAudio server happily takes a sink name on
        // open — callers obtain the name out-of-band (`pactl list sinks
        // short`). `None` keeps the historical default-sink behaviour.
        let dev_cstring = match req.device.as_deref() {
            Some(name) => Some(CString::new(name).map_err(|_| Error::DeviceOpen {
                backend: "pulse",
                detail: "device id contains an interior NUL byte".into(),
            })?),
            None => None,
        };
        let dev_ptr = dev_cstring
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(ptr::null());

        // Bytes per frame at the agreed sample format. `pa_buffer_attr`
        // is byte-denominated, so a frame-count hint needs the per-frame
        // byte size to translate. F32 = 4 bytes × channels.
        let bytes_per_frame = (channels as usize) * 4;
        // Honour `StreamRequest::buffer_frames` as a server-side hint:
        // when present, fill a `pa_buffer_attr` with `tlength` set to
        // the requested byte count and `minreq` set to roughly one
        // period (we keep ~20 ms when the hint is large, but never less
        // than the hint itself so the server doesn't ask for refills
        // smaller than the worker writes at). The other three fields
        // stay sentinel (`u32::MAX`) so the server picks defaults for
        // them, matching what NULL would have done. Without the hint we
        // continue to pass NULL so the server picks every field.
        let attr_storage: Option<pa_buffer_attr> = req
            .buffer_frames
            .map(|frames| make_buffer_attr(frames, req.sample_rate, bytes_per_frame));
        let attr_ptr: *const pa_buffer_attr = attr_storage
            .as_ref()
            .map(|a| a as *const pa_buffer_attr)
            .unwrap_or(ptr::null());

        let handle = unsafe {
            let mut err: c_int = 0;
            let s = (l.pa_simple_new)(
                ptr::null(),
                app.as_ptr(),
                PA_STREAM_PLAYBACK,
                dev_ptr,
                stream_name.as_ptr(),
                &spec,
                ptr::null(),
                attr_ptr,
                &mut err,
            );
            if s.is_null() {
                return Err(Error::DeviceOpen {
                    backend: "pulse",
                    detail: l.strerror(err),
                });
            }
            s
        };

        // Worker period follows the hint when one was given so the
        // server-side `minreq` and the client-side write size stay
        // aligned. Without a hint we keep the historical ~20 ms target
        // that matches the ALSA backend.
        let period_frames = req
            .buffer_frames
            .map(|f| (f as usize).max(64))
            .unwrap_or_else(|| ((req.sample_rate as usize) / 50).max(64));
        let paused = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let frames_played = Arc::new(AtomicU64::new(0));
        // Microseconds, published by the worker after each write.
        let latency_usec = Arc::new(AtomicU64::new(0));

        let state = PulseWorkerState {
            lib: l.clone(),
            handle: PaPtr(handle),
            cb,
            period_frames,
            channels: channels as usize,
            stop: stop.clone(),
            paused: paused.clone(),
            frames_played: frames_played.clone(),
            latency_usec: latency_usec.clone(),
        };

        let thread = std::thread::Builder::new()
            .name("oxideav-sysaudio-pulse".into())
            .spawn(move || state.run())
            .map_err(|e| {
                // Worker failed to spawn — clean the stream up before bailing.
                unsafe { (l.pa_simple_free)(handle) };
                Error::Runtime {
                    backend: "pulse",
                    detail: format!("spawn worker: {e}"),
                }
            })?;

        Ok(Box::new(PulseStream {
            lib: l,
            handle: PaPtr(handle),
            paused,
            stop,
            thread: Some(thread),
            latency_usec,
            format: StreamFormat {
                sample_rate: req.sample_rate,
                channels,
                format: SampleFormat::F32,
            },
        }))
    }
}

#[derive(Copy, Clone)]
struct PaPtr(*mut pa_simple);
unsafe impl Send for PaPtr {}

struct PulseWorkerState {
    lib: Arc<PulseLib>,
    handle: PaPtr,
    cb: Callback,
    period_frames: usize,
    channels: usize,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    frames_played: Arc<AtomicU64>,
    latency_usec: Arc<AtomicU64>,
}

impl PulseWorkerState {
    fn run(mut self) {
        let mut buf = vec![0.0f32; self.period_frames * self.channels];
        let bytes = self.period_frames * self.channels * 4;

        while !self.stop.load(Ordering::Relaxed) {
            if self.paused.load(Ordering::Relaxed) {
                // Keep the PulseAudio buffer well-fed with silence so
                // the server doesn't stall out — cork would need the
                // full async API.
                for s in buf.iter_mut() {
                    *s = 0.0;
                }
                // Skip the callback entirely while paused.
            } else {
                let info = CallbackInfo {
                    frames_played: self.frames_played.load(Ordering::Relaxed),
                };
                (self.cb)(&mut buf, &info);
            }

            unsafe {
                let mut err: c_int = 0;
                let r = (self.lib.pa_simple_write)(
                    self.handle.0,
                    buf.as_ptr() as *const c_void,
                    bytes,
                    &mut err,
                );
                if r < 0 {
                    // Pulse server went away or similar — bail out.
                    return;
                }
            }
            if !self.paused.load(Ordering::Relaxed) {
                self.frames_played
                    .fetch_add(self.period_frames as u64, Ordering::Relaxed);
            }

            // Publish the server's view of end-to-end latency. This is
            // what includes network / Bluetooth / sink delays, so the
            // player gets a useful value for A/V sync compensation.
            unsafe {
                let mut err: c_int = 0;
                let usec = (self.lib.pa_simple_get_latency)(self.handle.0, &mut err);
                if usec != u64::MAX {
                    self.latency_usec.store(usec, Ordering::Relaxed);
                }
            }
        }
    }
}

struct PulseStream {
    lib: Arc<PulseLib>,
    handle: PaPtr,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    latency_usec: Arc<AtomicU64>,
    format: StreamFormat,
}

unsafe impl Send for PulseStream {}

impl StreamImpl for PulseStream {
    fn play(&mut self) -> Result<()> {
        self.paused.store(false, Ordering::Relaxed);
        Ok(())
    }
    fn pause(&mut self) -> Result<()> {
        self.paused.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn format(&self) -> StreamFormat {
        self.format
    }
    fn latency(&self) -> Option<Duration> {
        let usec = self.latency_usec.load(Ordering::Relaxed);
        Some(Duration::from_micros(usec))
    }
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            let mut err: c_int = 0;
            let _ = (self.lib.pa_simple_flush)(self.handle.0, &mut err);
            (self.lib.pa_simple_free)(self.handle.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_attr_basic_48k_stereo() {
        // 4_800 frames × 8 bytes/frame (stereo f32) = 38_400 bytes for
        // tlength. minreq is ~20 ms (= 4_800 frames @48k / 50) = 960
        // frames × 8 bytes = 7_680 bytes, capped at tlength.
        let attr = make_buffer_attr(4_800, 48_000, 8);
        assert_eq!(attr.tlength, 4_800 * 8);
        assert_eq!(attr.minreq, 960 * 8);
        assert_eq!(attr.maxlength, u32::MAX);
        assert_eq!(attr.prebuf, u32::MAX);
        assert_eq!(attr.fragsize, u32::MAX);
    }

    #[test]
    fn buffer_attr_minreq_capped_at_tlength() {
        // Tiny hint: 32 frames × 8 = 256 bytes. The ~20 ms one-period
        // computation would yield 64 frames floor (since 48k/50 = 960,
        // but max(64) gates the lower end) which is still 64 × 8 = 512
        // bytes — strictly larger than tlength, so the cap kicks in and
        // pulls minreq down to tlength.
        let attr = make_buffer_attr(32, 48_000, 8);
        assert_eq!(attr.tlength, 256);
        assert_eq!(attr.minreq, 256);
    }

    #[test]
    fn buffer_attr_saturates_huge_request() {
        // A frame count whose byte product overflows u32 must clamp at
        // u32::MAX rather than wrapping, otherwise the server would see
        // a small bogus tlength.
        let attr = make_buffer_attr(u32::MAX, 48_000, 8);
        assert_eq!(attr.tlength, u32::MAX);
    }

    #[test]
    fn buffer_attr_mono_low_rate() {
        // 8 kHz mono, 80 frames (= 10 ms): tlength = 80 × 4 = 320 bytes.
        // One period at 8 kHz = max(64, 160) = 160 frames → 640 bytes,
        // capped at tlength → minreq = 320.
        let attr = make_buffer_attr(80, 8_000, 4);
        assert_eq!(attr.tlength, 320);
        assert_eq!(attr.minreq, 320);
    }
}
