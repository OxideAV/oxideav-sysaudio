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

type Fn_pa_simple_new = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const pa_sample_spec,
    map: *const c_void,
    attr: *const c_void,
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
                ptr::null(),
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
        let handle = unsafe {
            let mut err: c_int = 0;
            let s = (l.pa_simple_new)(
                ptr::null(),
                app.as_ptr(),
                PA_STREAM_PLAYBACK,
                ptr::null(),
                stream_name.as_ptr(),
                &spec,
                ptr::null(),
                ptr::null(),
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

        // ~20 ms period — same target as ALSA so latency feels the same.
        let period_frames = ((req.sample_rate as usize) / 50).max(64);
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
