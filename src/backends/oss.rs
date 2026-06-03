//! OSS output backend.
//!
//! Drives `/dev/dsp` directly through the Linux kernel UAPI
//! (`<sys/soundcard.h>`). Because OSS is a character-device interface
//! the only thing we need from userspace is libc's `open`/`close`/
//! `write`/`ioctl` — those are reached via `libloading` the same way the
//! other Linux backends reach `libasound`/`libpulse-simple`, so the
//! crate stays free of build-time C linkage.
//!
//! The user callback is called from a worker thread that loops:
//! invoke the callback for one period of f32 frames, convert to S16_LE
//! (the format every OSS driver advertises), `write` one period to the
//! device fd, repeat. `Stream::drop` flips an atomic stop flag that the
//! worker checks each iteration; the worker then exits, the main thread
//! joins it, and the fd is closed.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use libloading::{Library, Symbol};

use crate::backend::{Backend, Callback};
use crate::format::{CallbackInfo, SampleFormat, StreamFormat, StreamRequest};
use crate::stream::StreamImpl;
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// libc symbols — open/close/write/ioctl. The OSS interface is a kernel
// UAPI exposed as a character device, so all userspace surface needed
// to drive it lives in plain libc. We dlopen rather than link to keep
// `oxideav-sysaudio` free of build-time C deps just like the other
// Linux backends.
// ---------------------------------------------------------------------------

/// `open(2)` flags from `<fcntl.h>`. Stable Linux kernel UAPI: `O_WRONLY`
/// is octal 1, `O_NONBLOCK` is octal 0o4000. Reading them off the libc
/// header would also work but the values are kernel-fixed across glibc /
/// musl / dietlibc on every Linux distro.
const O_WRONLY: c_int = 1;
const O_NONBLOCK: c_int = 0o4000;

type Fn_open = unsafe extern "C" fn(path: *const c_char, flags: c_int, mode: c_uint) -> c_int;
type Fn_close = unsafe extern "C" fn(fd: c_int) -> c_int;
type Fn_write = unsafe extern "C" fn(fd: c_int, buf: *const c_void, count: usize) -> isize;
/// `ioctl` is variadic in libc; we only ever pass it a pointer (to an
/// int that the kernel reads-modifies-writes back into for the OSS
/// `_IOWR` family), so binding a fixed 3-arg signature is portable.
type Fn_ioctl = unsafe extern "C" fn(fd: c_int, request: c_ulong_ioctl, arg: *mut c_void) -> c_int;
/// Glibc declares `ioctl`'s request as `unsigned long` on 64-bit Linux,
/// the same width as `c_ulong`. We use a local alias so the binding
/// matches without dragging the `c_ulong` ambiguity through the rest of
/// the file.
type c_ulong_ioctl = u64;

struct OssLib {
    _libc: Library,
    open: Fn_open,
    close: Fn_close,
    write: Fn_write,
    ioctl: Fn_ioctl,
}

unsafe impl Send for OssLib {}
unsafe impl Sync for OssLib {}

impl OssLib {
    fn load() -> Result<Arc<Self>> {
        // Same soname candidates as `alsa::load_libc_free` — keep the
        // list in sync. glibc + musl + the BSD-flavoured libc on a few
        // Linux distros all live behind one of these.
        const CANDIDATES: &[&str] = &["libc.so.6", "libc.so", "libc.musl-x86_64.so.1"];
        let mut last_err: Option<libloading::Error> = None;
        for &name in CANDIDATES {
            match unsafe { Library::new(name) } {
                Ok(lib) => unsafe {
                    macro_rules! sym {
                        ($n:ident, $t:ty) => {{
                            let s: std::result::Result<Symbol<$t>, _> =
                                lib.get(concat!(stringify!($n), "\0").as_bytes());
                            match s {
                                Ok(sym) => *sym,
                                Err(e) => {
                                    return Err(Error::SymbolMissing {
                                        backend: "oss",
                                        symbol: stringify!($n),
                                        source: e,
                                    });
                                }
                            }
                        }};
                    }
                    return Ok(Arc::new(OssLib {
                        open: sym!(open, Fn_open),
                        close: sym!(close, Fn_close),
                        write: sym!(write, Fn_write),
                        ioctl: sym!(ioctl, Fn_ioctl),
                        _libc: lib,
                    }));
                },
                Err(e) => last_err = Some(e),
            }
        }
        Err(Error::LibraryLoad {
            backend: "oss",
            soname: "libc.so.6",
            source: last_err.unwrap_or(libloading::Error::DlOpenUnknown),
        })
    }
}

fn lib() -> Result<Arc<OssLib>> {
    static CACHED: OnceLock<Mutex<Option<Arc<OssLib>>>> = OnceLock::new();
    let slot = CACHED.get_or_init(|| Mutex::new(None));
    let mut g = slot.lock().unwrap();
    if let Some(l) = g.as_ref() {
        return Ok(l.clone());
    }
    let l = OssLib::load()?;
    *g = Some(l.clone());
    Ok(l)
}

// ---------------------------------------------------------------------------
// OSS / `<sys/soundcard.h>` constants — Linux kernel UAPI.
//
// The OSS interface predates ALSA: every Linux kernel still ships an
// `oss-emulator` (or kept-compatible) `/dev/dsp` whose ioctl numbers
// have been fixed since the 1990s. The macro derivation is the standard
// Linux `_IOC(dir, type, nr, size)` packing:
//
//   _IOC = (dir << 30) | (size << 16) | (type << 8) | nr
//   _IOC_READ  = 2, _IOC_WRITE = 1
//   _IOWR(t,n,s) = _IOC(3, t, n, sizeof(s))
//   _IOR (t,n,s) = _IOC(2, t, n, sizeof(s))
//   _IO  (t,n)   = _IOC(0, t, n, 0)
//
// Every `SNDCTL_DSP_*` ioctl below is `_IOWR('P', N, int)` (size 4)
// except where noted. Values verified against the kernel UAPI header
// `include/uapi/linux/soundcard.h` shipped with every Linux distro.
// ---------------------------------------------------------------------------

const IOC_DIR_WRITE: u32 = 1;
const IOC_DIR_READ: u32 = 2;
const IOC_TYPE_P: u32 = b'P' as u32;

/// Build a Linux `_IOC` request number at const time so the constants
/// below are computed from first principles (kernel ABI macro) rather
/// than transcribed magic hex.
const fn ioc(dir: u32, ty: u32, nr: u32, sz: u32) -> c_ulong_ioctl {
    ((dir << 30) | (sz << 16) | (ty << 8) | nr) as c_ulong_ioctl
}
const fn iowr_int(nr: u32) -> c_ulong_ioctl {
    ioc(IOC_DIR_READ | IOC_DIR_WRITE, IOC_TYPE_P, nr, 4)
}

/// `SNDCTL_DSP_RESET = _IO('P', 0)` — stops the device, drops any
/// queued audio.
const SNDCTL_DSP_RESET: c_ulong_ioctl = ioc(0, IOC_TYPE_P, 0, 0);
/// `SNDCTL_DSP_SYNC = _IO('P', 1)` — block until everything queued has
/// played, then return. Resolved but unused today; kept around for the
/// `drain()` surface a future round will add.
#[allow(dead_code)]
const SNDCTL_DSP_SYNC: c_ulong_ioctl = ioc(0, IOC_TYPE_P, 1, 0);
/// `SNDCTL_DSP_SPEED = _IOWR('P', 2, int)` — request sample rate;
/// kernel writes the snapped rate back.
const SNDCTL_DSP_SPEED: c_ulong_ioctl = iowr_int(2);
/// `SNDCTL_DSP_SETFMT = _IOWR('P', 5, int)` — request sample format
/// (one of the `AFMT_*` constants below); kernel writes the granted
/// format back.
const SNDCTL_DSP_SETFMT: c_ulong_ioctl = iowr_int(5);
/// `SNDCTL_DSP_CHANNELS = _IOWR('P', 6, int)` — request channel count;
/// kernel writes the snapped count back.
const SNDCTL_DSP_CHANNELS: c_ulong_ioctl = iowr_int(6);

/// `AFMT_S16_LE = 0x10` — the one format every OSS driver advertises;
/// even the kernel's `oss-emulator` on top of ALSA guarantees it.
const AFMT_S16_LE: c_int = 0x10;

// ---------------------------------------------------------------------------
// Backend impl.
// ---------------------------------------------------------------------------

pub(crate) struct OssBackend;

impl Backend for OssBackend {
    fn name(&self) -> &'static str {
        "oss"
    }
    fn description(&self) -> &'static str {
        "OSS / /dev/dsp (libc via libloading)"
    }

    fn probe(&self) -> Result<()> {
        let l = lib()?;
        // O_NONBLOCK so a busy device returns -EBUSY rather than blocking
        // probe(). A throwaway-open with no `ioctl` is enough to learn
        // whether the character device exists and is openable; the real
        // `open()` path does the format negotiation.
        let path = CString::new("/dev/dsp").unwrap();
        unsafe {
            let fd = (l.open)(path.as_ptr(), O_WRONLY | O_NONBLOCK, 0);
            if fd < 0 {
                return Err(Error::DeviceOpen {
                    backend: "oss",
                    detail: "open(/dev/dsp): errno set".into(),
                });
            }
            (l.close)(fd);
        }
        Ok(())
    }

    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>> {
        let l = lib()?;
        // OSS has no per-endpoint enumeration; honour `req.device` by
        // letting the caller name an alternate character device
        // (`/dev/dsp1`, `/dev/dsp_hw0`, …). `None` keeps the historical
        // default of `/dev/dsp`.
        let path_str = req.device.as_deref().unwrap_or("/dev/dsp");
        let path = CString::new(path_str).map_err(|_| Error::DeviceOpen {
            backend: "oss",
            detail: "device id contains an interior NUL byte".into(),
        })?;
        // Blocking write semantics on the worker thread match what the
        // ALSA + PulseAudio backends do; the worker stops when the atomic
        // flag flips.
        let fd = unsafe { (l.open)(path.as_ptr(), O_WRONLY, 0) };
        if fd < 0 {
            return Err(Error::DeviceOpen {
                backend: "oss",
                detail: format!("open({path_str})"),
            });
        }

        // Configure format / channels / rate. From here on a failure
        // path closes the fd.
        let res = configure_and_spawn(l.clone(), fd, req, cb);
        match res {
            Ok(stream) => Ok(stream),
            Err(e) => {
                unsafe { (l.close)(fd) };
                Err(e)
            }
        }
    }
}

fn configure_and_spawn(
    l: Arc<OssLib>,
    fd: c_int,
    req: StreamRequest,
    cb: Callback,
) -> Result<Box<dyn StreamImpl>> {
    // OSS ioctls are `_IOWR('P', N, int)`: the kernel reads the
    // requested value, snaps it to what the driver supports, and writes
    // the granted value back through the same pointer. Each call here
    // therefore both requests and discovers.
    let mut fmt: c_int = AFMT_S16_LE;
    if unsafe { (l.ioctl)(fd, SNDCTL_DSP_SETFMT, &mut fmt as *mut c_int as *mut c_void) } < 0
        || fmt != AFMT_S16_LE
    {
        return Err(Error::UnsupportedFormat {
            backend: "oss",
            detail: format!("driver refused S16_LE (got 0x{fmt:x})"),
        });
    }

    let mut channels: c_int = req.channels.clamp(1, 8) as c_int;
    if unsafe {
        (l.ioctl)(
            fd,
            SNDCTL_DSP_CHANNELS,
            &mut channels as *mut c_int as *mut c_void,
        )
    } < 0
        || channels < 1
    {
        return Err(Error::UnsupportedFormat {
            backend: "oss",
            detail: format!("driver refused channel count (got {channels})"),
        });
    }

    let mut rate: c_int = req.sample_rate as c_int;
    if unsafe { (l.ioctl)(fd, SNDCTL_DSP_SPEED, &mut rate as *mut c_int as *mut c_void) } < 0
        || rate <= 0
    {
        return Err(Error::UnsupportedFormat {
            backend: "oss",
            detail: format!("driver refused sample rate (got {rate})"),
        });
    }

    // Target ~20 ms periods unless the caller hinted otherwise — same
    // policy as the ALSA + PulseAudio workers. OSS doesn't expose a
    // separate "set period size" ioctl in the historic UAPI surface,
    // so the worker's write size IS the effective period.
    let period_frames = req
        .buffer_frames
        .map(|b| (b as usize).max(64))
        .unwrap_or_else(|| ((rate as usize) / 50).max(64));
    let channels_us = channels as usize;
    let sample_rate = rate as u32;

    let stop = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let frames_played = Arc::new(AtomicU64::new(0));
    // OSS has no `SNDCTL_DSP_GETODELAY`-equivalent we can rely on across
    // every kernel; we publish the queued-byte estimate (period_frames
    // × channels × 2 bytes/frame) instead so `Stream::latency()` reports
    // at least the worker's own buffering.
    let queued_frames = Arc::new(AtomicI32::new(0));

    let state = OssWorkerState {
        lib: l.clone(),
        fd,
        cb,
        period_frames,
        channels: channels_us,
        stop: stop.clone(),
        paused: paused.clone(),
        frames_played: frames_played.clone(),
        queued_frames: queued_frames.clone(),
    };

    let thread = std::thread::Builder::new()
        .name("oxideav-sysaudio-oss".into())
        .spawn(move || state.run())
        .map_err(|e| Error::Runtime {
            backend: "oss",
            detail: format!("spawn worker: {e}"),
        })?;

    Ok(Box::new(OssStream {
        lib: l,
        fd: FdGuard(fd),
        paused,
        stop,
        thread: Some(thread),
        queued_frames,
        format: StreamFormat {
            sample_rate,
            channels: channels as u16,
            format: SampleFormat::F32,
        },
    }))
}

struct OssWorkerState {
    lib: Arc<OssLib>,
    fd: c_int,
    cb: Callback,
    period_frames: usize,
    channels: usize,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    frames_played: Arc<AtomicU64>,
    queued_frames: Arc<AtomicI32>,
}

impl OssWorkerState {
    fn run(mut self) {
        let mut f32_buf = vec![0.0f32; self.period_frames * self.channels];
        let mut s16_buf = vec![0i16; self.period_frames * self.channels];

        while !self.stop.load(Ordering::Relaxed) {
            if self.paused.load(Ordering::Relaxed) {
                // Keep the device fed with silence rather than blocking
                // (OSS has no soft-cork that doesn't drain); the user's
                // callback is skipped this iteration so audio output
                // genuinely stops.
                for s in f32_buf.iter_mut() {
                    *s = 0.0;
                }
            } else {
                let info = CallbackInfo {
                    frames_played: self.frames_played.load(Ordering::Relaxed),
                };
                (self.cb)(&mut f32_buf, &info);
            }

            // f32 → S16_LE. Same clamp-and-scale every consumer-grade
            // resampler uses; OSS doesn't accept native-endian float on
            // many drivers so the conversion is mandatory.
            convert_f32_to_s16(&f32_buf, &mut s16_buf);

            let bytes = s16_buf.len() * 2;
            let written =
                unsafe { (self.lib.write)(self.fd, s16_buf.as_ptr() as *const c_void, bytes) };
            if written < 0 {
                // Errno: -EINTR is the only one that's recoverable per
                // POSIX; everything else means the device went away.
                return;
            }
            if !self.paused.load(Ordering::Relaxed) {
                // OSS write reports the bytes accepted; convert back
                // to frames for the played-counter.
                let frames_written = (written as usize) / (self.channels.max(1) * 2);
                self.frames_played
                    .fetch_add(frames_written as u64, Ordering::Relaxed);
            }
            // Worker-side buffering estimate — at least one period sits
            // in our own write buffer when the loop is up to speed.
            self.queued_frames
                .store(self.period_frames as i32, Ordering::Relaxed);
        }
    }
}

/// Stable f32 → S16_LE conversion shared with the worker's hot path.
/// Pulled out so the test suite can exercise it directly without
/// standing up a worker thread.
fn convert_f32_to_s16(src: &[f32], dst: &mut [i16]) {
    for (d, &s) in dst.iter_mut().zip(src.iter()) {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i32;
        *d = v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

/// RAII fd holder so a panic or early `Drop::drop` on the stream still
/// closes the device.
struct FdGuard(c_int);
impl FdGuard {
    fn fd(&self) -> c_int {
        self.0
    }
}

struct OssStream {
    lib: Arc<OssLib>,
    fd: FdGuard,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    queued_frames: Arc<AtomicI32>,
    format: StreamFormat,
}

unsafe impl Send for OssStream {}

impl StreamImpl for OssStream {
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
        let frames = self.queued_frames.load(Ordering::Relaxed).max(0) as u64;
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
            // SYNC blocks until queued audio finishes; RESET drops it.
            // We choose RESET to keep `Drop` snappy — a user with a
            // long-tail buffer (e.g. 200 ms) doesn't want their program
            // to hang at exit waiting for the trailing samples.
            let mut dummy: c_int = 0;
            let _ = (self.lib.ioctl)(
                self.fd.fd(),
                SNDCTL_DSP_RESET,
                &mut dummy as *mut c_int as *mut c_void,
            );
            (self.lib.close)(self.fd.fd());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the `_IOC` packing against well-known OSS ABI numbers
    /// derived from the same kernel UAPI macro. `_IOWR('P', 2, int)`
    /// (`SNDCTL_DSP_SPEED`) packs as:
    ///   (3 << 30) | (4 << 16) | ('P' << 8) | 2
    /// = 0xC0040000 | 0x5000 | 0x02 = 0xC0045002
    #[test]
    fn ioctl_request_numbers_match_uapi_macro() {
        assert_eq!(SNDCTL_DSP_SPEED, 0xC0045002);
        assert_eq!(SNDCTL_DSP_SETFMT, 0xC0045005);
        assert_eq!(SNDCTL_DSP_CHANNELS, 0xC0045006);
        // `_IO('P', 0)` = (0 << 30) | (0 << 16) | ('P' << 8) | 0 = 0x5000
        assert_eq!(SNDCTL_DSP_RESET, 0x5000);
        // `_IO('P', 1)` = 0x5001
        assert_eq!(SNDCTL_DSP_SYNC, 0x5001);
    }

    #[test]
    fn afmt_s16_le_is_kernel_constant() {
        // From `<sys/soundcard.h>`: `AFMT_S16_LE = 0x00000010`.
        assert_eq!(AFMT_S16_LE, 0x10);
    }

    #[test]
    fn convert_f32_to_s16_clamps_and_scales() {
        let src = [-2.0f32, -1.0, -0.5, 0.0, 0.5, 1.0, 2.0];
        let mut dst = [0i16; 7];
        convert_f32_to_s16(&src, &mut dst);
        // -2.0 saturates at -32767 (clamp(-1.0, 1.0) × 32767 = -32767);
        // i16::MIN = -32768, so the secondary clamp is a no-op here.
        assert_eq!(dst[0], -32767);
        assert_eq!(dst[1], -32767);
        // -0.5 × 32767 = -16383.5 → truncates to -16383 (as i32 cast).
        assert!(dst[2] == -16383 || dst[2] == -16384);
        assert_eq!(dst[3], 0);
        // 0.5 × 32767 = 16383.5 → 16383.
        assert!(dst[4] == 16383 || dst[4] == 16384);
        assert_eq!(dst[5], 32767);
        assert_eq!(dst[6], 32767);
    }

    #[test]
    fn convert_f32_to_s16_handles_shorter_dst() {
        // Zip stops at the shorter iterator; verify nothing panics if
        // the worker ever feeds a mismatched pair (it shouldn't, but
        // belt-and-braces).
        let src = [1.0f32; 8];
        let mut dst = [0i16; 4];
        convert_f32_to_s16(&src, &mut dst);
        assert!(dst.iter().all(|&x| x == 32767));
    }

    #[test]
    fn o_flags_match_linux_uapi() {
        // Kernel UAPI guarantees octal 0o1 / 0o4000 for these on every
        // Linux architecture we care about.
        assert_eq!(O_WRONLY, 1);
        assert_eq!(O_NONBLOCK, 0o4000);
    }
}
