//! ALSA output backend.
//!
//! Loads `libasound.so.2` at runtime through `libloading` and drives a
//! playback PCM via the `snd_pcm_*` family. Because ALSA's blocking
//! `snd_pcm_writei` doesn't naturally fit a pull-callback model, we
//! spawn a worker thread that loops: invoke the user callback, convert
//! to the device's format if needed, write one period, repeat. Stream
//! drop signals the worker to exit and joins.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use libloading::{Library, Symbol};

use crate::backend::{Backend, Callback};
use crate::format::{CallbackInfo, SampleFormat, StreamFormat, StreamRequest};
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

// Function-pointer type aliases, kept with ALSA's spelling.
type Fn_snd_pcm_open = unsafe extern "C" fn(
    pcmp: *mut *mut snd_pcm_t,
    name: *const c_char,
    stream: c_int,
    mode: c_int,
) -> c_int;
type Fn_snd_pcm_close = unsafe extern "C" fn(pcm: *mut snd_pcm_t) -> c_int;
type Fn_snd_pcm_prepare = unsafe extern "C" fn(pcm: *mut snd_pcm_t) -> c_int;
type Fn_snd_pcm_writei = unsafe extern "C" fn(
    pcm: *mut snd_pcm_t,
    buffer: *const c_void,
    size: c_ulong,
) -> c_long;
type Fn_snd_pcm_recover =
    unsafe extern "C" fn(pcm: *mut snd_pcm_t, err: c_int, silent: c_int) -> c_int;
type Fn_snd_pcm_drop = unsafe extern "C" fn(pcm: *mut snd_pcm_t) -> c_int;
type Fn_snd_pcm_pause = unsafe extern "C" fn(pcm: *mut snd_pcm_t, enable: c_int) -> c_int;
type Fn_snd_pcm_delay =
    unsafe extern "C" fn(pcm: *mut snd_pcm_t, delayp: *mut c_long) -> c_int;
type Fn_snd_strerror = unsafe extern "C" fn(errnum: c_int) -> *const c_char;

type Fn_hw_params_malloc =
    unsafe extern "C" fn(ptr: *mut *mut snd_pcm_hw_params_t) -> c_int;
type Fn_hw_params_free = unsafe extern "C" fn(obj: *mut snd_pcm_hw_params_t);
type Fn_hw_params_any =
    unsafe extern "C" fn(pcm: *mut snd_pcm_t, params: *mut snd_pcm_hw_params_t) -> c_int;
type Fn_hw_params_set_access = unsafe extern "C" fn(
    pcm: *mut snd_pcm_t,
    params: *mut snd_pcm_hw_params_t,
    access: c_int,
) -> c_int;
type Fn_hw_params_set_format = unsafe extern "C" fn(
    pcm: *mut snd_pcm_t,
    params: *mut snd_pcm_hw_params_t,
    format: c_int,
) -> c_int;
type Fn_hw_params_set_channels = unsafe extern "C" fn(
    pcm: *mut snd_pcm_t,
    params: *mut snd_pcm_hw_params_t,
    val: c_uint,
) -> c_int;
type Fn_hw_params_set_rate_near = unsafe extern "C" fn(
    pcm: *mut snd_pcm_t,
    params: *mut snd_pcm_hw_params_t,
    val: *mut c_uint,
    dir: *mut c_int,
) -> c_int;
type Fn_hw_params_set_period_size_near = unsafe extern "C" fn(
    pcm: *mut snd_pcm_t,
    params: *mut snd_pcm_hw_params_t,
    val: *mut c_ulong,
    dir: *mut c_int,
) -> c_int;
type Fn_hw_params_set_buffer_size_near = unsafe extern "C" fn(
    pcm: *mut snd_pcm_t,
    params: *mut snd_pcm_hw_params_t,
    val: *mut c_ulong,
) -> c_int;
type Fn_hw_params_get_period_size = unsafe extern "C" fn(
    params: *const snd_pcm_hw_params_t,
    val: *mut c_ulong,
    dir: *mut c_int,
) -> c_int;
type Fn_hw_params = unsafe extern "C" fn(
    pcm: *mut snd_pcm_t,
    params: *mut snd_pcm_hw_params_t,
) -> c_int;

// ---------------------------------------------------------------------------
// Loaded library + resolved symbols.
// ---------------------------------------------------------------------------

struct AlsaLib {
    _lib: Library,
    snd_pcm_open: Fn_snd_pcm_open,
    snd_pcm_close: Fn_snd_pcm_close,
    snd_pcm_prepare: Fn_snd_pcm_prepare,
    snd_pcm_writei: Fn_snd_pcm_writei,
    snd_pcm_recover: Fn_snd_pcm_recover,
    snd_pcm_drop: Fn_snd_pcm_drop,
    snd_pcm_pause: Fn_snd_pcm_pause,
    snd_pcm_delay: Fn_snd_pcm_delay,
    snd_strerror: Fn_snd_strerror,

    hw_params_malloc: Fn_hw_params_malloc,
    hw_params_free: Fn_hw_params_free,
    hw_params_any: Fn_hw_params_any,
    hw_params_set_access: Fn_hw_params_set_access,
    hw_params_set_format: Fn_hw_params_set_format,
    hw_params_set_channels: Fn_hw_params_set_channels,
    hw_params_set_rate_near: Fn_hw_params_set_rate_near,
    hw_params_set_period_size_near: Fn_hw_params_set_period_size_near,
    hw_params_set_buffer_size_near: Fn_hw_params_set_buffer_size_near,
    hw_params_get_period_size: Fn_hw_params_get_period_size,
    hw_params: Fn_hw_params,
}

// SAFETY: libalsa's `snd_pcm_*` calls are thread-safe per-handle; we
// never share a handle across threads without a Mutex. The function
// pointers themselves are immutable after load.
unsafe impl Send for AlsaLib {}
unsafe impl Sync for AlsaLib {}

impl AlsaLib {
    fn load() -> Result<Arc<Self>> {
        unsafe {
            let lib = Library::new("libasound.so.2").map_err(|e| Error::LibraryLoad {
                backend: "alsa",
                soname: "libasound.so.2",
                source: e,
            })?;
            macro_rules! sym {
                ($name:ident, $ty:ty) => {{
                    let s: Symbol<$ty> =
                        lib.get(concat!(stringify!($name), "\0").as_bytes())
                            .map_err(|e| Error::SymbolMissing {
                                backend: "alsa",
                                symbol: stringify!($name),
                                source: e,
                            })?;
                    // Deref copies the fn-pointer out; the borrow on
                    // `lib` ends here, and the fn pointer remains valid
                    // as long as we keep the Library alive in AlsaLib.
                    *s
                }};
            }
            let snd_pcm_open = sym!(snd_pcm_open, Fn_snd_pcm_open);
            let snd_pcm_close = sym!(snd_pcm_close, Fn_snd_pcm_close);
            let snd_pcm_prepare = sym!(snd_pcm_prepare, Fn_snd_pcm_prepare);
            let snd_pcm_writei = sym!(snd_pcm_writei, Fn_snd_pcm_writei);
            let snd_pcm_recover = sym!(snd_pcm_recover, Fn_snd_pcm_recover);
            let snd_pcm_drop = sym!(snd_pcm_drop, Fn_snd_pcm_drop);
            let snd_pcm_pause = sym!(snd_pcm_pause, Fn_snd_pcm_pause);
            let snd_pcm_delay = sym!(snd_pcm_delay, Fn_snd_pcm_delay);
            let snd_strerror = sym!(snd_strerror, Fn_snd_strerror);

            let hw_params_malloc = sym!(snd_pcm_hw_params_malloc, Fn_hw_params_malloc);
            let hw_params_free = sym!(snd_pcm_hw_params_free, Fn_hw_params_free);
            let hw_params_any = sym!(snd_pcm_hw_params_any, Fn_hw_params_any);
            let hw_params_set_access =
                sym!(snd_pcm_hw_params_set_access, Fn_hw_params_set_access);
            let hw_params_set_format =
                sym!(snd_pcm_hw_params_set_format, Fn_hw_params_set_format);
            let hw_params_set_channels =
                sym!(snd_pcm_hw_params_set_channels, Fn_hw_params_set_channels);
            let hw_params_set_rate_near =
                sym!(snd_pcm_hw_params_set_rate_near, Fn_hw_params_set_rate_near);
            let hw_params_set_period_size_near = sym!(
                snd_pcm_hw_params_set_period_size_near,
                Fn_hw_params_set_period_size_near
            );
            let hw_params_set_buffer_size_near = sym!(
                snd_pcm_hw_params_set_buffer_size_near,
                Fn_hw_params_set_buffer_size_near
            );
            let hw_params_get_period_size =
                sym!(snd_pcm_hw_params_get_period_size, Fn_hw_params_get_period_size);
            let hw_params = sym!(snd_pcm_hw_params, Fn_hw_params);

            Ok(Arc::new(AlsaLib {
                _lib: lib,
                snd_pcm_open,
                snd_pcm_close,
                snd_pcm_prepare,
                snd_pcm_writei,
                snd_pcm_recover,
                snd_pcm_drop,
                snd_pcm_pause,
                snd_pcm_delay,
                snd_strerror,
                hw_params_malloc,
                hw_params_free,
                hw_params_any,
                hw_params_set_access,
                hw_params_set_format,
                hw_params_set_channels,
                hw_params_set_rate_near,
                hw_params_set_period_size_near,
                hw_params_set_buffer_size_near,
                hw_params_get_period_size,
                hw_params,
            }))
        }
    }

    fn strerror(&self, err: c_int) -> String {
        unsafe {
            let p = (self.snd_strerror)(err);
            if p.is_null() {
                format!("alsa error {err}")
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

// Cached so we don't dlopen once per stream.
fn lib() -> Result<Arc<AlsaLib>> {
    static CACHED: OnceLock<Mutex<Option<Arc<AlsaLib>>>> = OnceLock::new();
    let slot = CACHED.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap();
    if let Some(l) = guard.as_ref() {
        return Ok(l.clone());
    }
    let l = AlsaLib::load()?;
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
        "ALSA (libasound.so.2 via libloading)"
    }

    fn probe(&self) -> Result<()> {
        let l = lib()?;
        unsafe {
            let mut pcm: *mut snd_pcm_t = ptr::null_mut();
            let name = CString::new("default").unwrap();
            let r = (l.snd_pcm_open)(
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
            (l.snd_pcm_close)(pcm);
        }
        Ok(())
    }

    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>> {
        let l = lib()?;
        unsafe { open_inner(l, req, cb) }
    }
}

unsafe fn open_inner(
    l: Arc<AlsaLib>,
    req: StreamRequest,
    cb: Callback,
) -> Result<Box<dyn StreamImpl>> {
    let name = CString::new("default").unwrap();
    let mut pcm: *mut snd_pcm_t = ptr::null_mut();
    let r = (l.snd_pcm_open)(&mut pcm, name.as_ptr(), SND_PCM_STREAM_PLAYBACK, 0);
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
            (l.snd_pcm_close)(pcm);
            Err(e)
        }
    }
}

unsafe fn configure_and_spawn(
    l: Arc<AlsaLib>,
    pcm: *mut snd_pcm_t,
    req: StreamRequest,
    cb: Callback,
) -> Result<Box<dyn StreamImpl>> {
    let mut hw: *mut snd_pcm_hw_params_t = ptr::null_mut();
    let r = (l.hw_params_malloc)(&mut hw);
    if r < 0 {
        return Err(Error::DeviceOpen {
            backend: "alsa",
            detail: format!("hw_params_malloc: {}", l.strerror(r)),
        });
    }
    let hw_guard = HwParamsGuard { l: l.clone(), hw };

    check(
        &l,
        (l.hw_params_any)(pcm, hw),
        "hw_params_any",
    )?;
    check(
        &l,
        (l.hw_params_set_access)(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED),
        "hw_params_set_access",
    )?;

    // Prefer FLOAT_LE; fall back to S16_LE for devices that don't
    // advertise float (uncommon on modern drivers, common on embedded).
    let (device_fmt, device_fmt_is_f32) =
        match (l.hw_params_set_format)(pcm, hw, SND_PCM_FORMAT_FLOAT_LE) {
            0 => (SND_PCM_FORMAT_FLOAT_LE, true),
            _ => {
                check(
                    &l,
                    (l.hw_params_set_format)(pcm, hw, SND_PCM_FORMAT_S16_LE),
                    "hw_params_set_format(S16_LE)",
                )?;
                (SND_PCM_FORMAT_S16_LE, false)
            }
        };
    let _ = device_fmt;

    let want_ch = req.channels.clamp(1, 8) as c_uint;
    check(
        &l,
        (l.hw_params_set_channels)(pcm, hw, want_ch),
        "hw_params_set_channels",
    )?;

    let mut rate = req.sample_rate;
    let mut dir: c_int = 0;
    check(
        &l,
        (l.hw_params_set_rate_near)(pcm, hw, &mut rate, &mut dir),
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
        (l.hw_params_set_period_size_near)(pcm, hw, &mut period, &mut dir2),
        "hw_params_set_period_size_near",
    )?;

    let mut buffer: c_ulong = period.saturating_mul(4);
    let _ = (l.hw_params_set_buffer_size_near)(pcm, hw, &mut buffer); // best-effort

    check(&l, (l.hw_params)(pcm, hw), "hw_params (commit)")?;

    let mut period_actual: c_ulong = 0;
    let mut pd: c_int = 0;
    let _ = (l.hw_params_get_period_size)(hw, &mut period_actual, &mut pd);
    drop(hw_guard);

    check(&l, (l.snd_pcm_prepare)(pcm), "snd_pcm_prepare")?;

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
        unsafe { (self.l.hw_params_free)(self.hw) }
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
                    (self.lib.snd_pcm_writei)(
                        self.pcm.0,
                        f32_buf.as_ptr() as *const c_void,
                        self.period_frames as c_ulong,
                    )
                } else {
                    for (d, &s) in s16_buf.iter_mut().zip(f32_buf.iter()) {
                        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i32;
                        *d = v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    }
                    (self.lib.snd_pcm_writei)(
                        self.pcm.0,
                        s16_buf.as_ptr() as *const c_void,
                        self.period_frames as c_ulong,
                    )
                }
            };
            if r < 0 {
                // -EPIPE / -ESTRPIPE / -EBADFD → ask ALSA to recover.
                let rec = unsafe { (self.lib.snd_pcm_recover)(self.pcm.0, r as c_int, 1) };
                if rec < 0 {
                    // Unrecoverable — bail.
                    return;
                }
                continue;
            }
            self.frames_played
                .fetch_add(r as u64, Ordering::Relaxed);

            // Publish driver-side queue depth for `Stream::latency()`.
            // Safe to call from this thread only (snd_pcm handles are
            // not thread-safe); readers get a stale-by-one-period view.
            let mut delay: c_long = 0;
            let dr = unsafe { (self.lib.snd_pcm_delay)(self.pcm.0, &mut delay) };
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
            let _ = (self.lib.snd_pcm_pause)(self.pcm.0, 0);
        }
        Ok(())
    }
    fn pause(&mut self) -> Result<()> {
        unsafe {
            let _ = (self.lib.snd_pcm_pause)(self.pcm.0, 1);
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
            let _ = (self.lib.snd_pcm_drop)(self.pcm.0);
            let _ = (self.lib.snd_pcm_close)(self.pcm.0);
        }
    }
}
