//! CoreAudio output backend via the AudioQueue API.
//!
//! Loads `AudioToolbox.framework` at runtime through `libloading`
//! (macOS frameworks are plain dylibs, so
//! `/System/Library/Frameworks/AudioToolbox.framework/AudioToolbox`
//! dlopen's cleanly). AudioQueue is the highest-level output API
//! CoreAudio ships: you hand it an ASBD plus a C callback and three
//! buffers, it invokes the callback from a CA-owned thread whenever a
//! buffer returns empty.
//!
//! The user's Rust `FnMut(&mut [f32], &CallbackInfo)` is boxed, stashed
//! in a heap-owned `CallbackState`, and reached through an
//! `extern "system"` trampoline.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use libloading::{Library, Symbol};

use crate::backend::{Backend, Callback};
use crate::format::{CallbackInfo, SampleFormat, StreamFormat, StreamRequest};
use crate::stream::StreamImpl;
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// CoreAudio types and constants.
// ---------------------------------------------------------------------------

type OSStatus = i32;

// kAudioFormatLinearPCM = FourCharCode('l','p','c','m') packed big-endian.
const kAudioFormatLinearPCM: u32 = 0x6C70_636D;
const kAudioFormatFlagIsFloat: u32 = 1 << 0;
const kAudioFormatFlagIsPacked: u32 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioStreamBasicDescription {
    mSampleRate: f64,
    mFormatID: u32,
    mFormatFlags: u32,
    mBytesPerPacket: u32,
    mFramesPerPacket: u32,
    mBytesPerFrame: u32,
    mChannelsPerFrame: u32,
    mBitsPerChannel: u32,
    mReserved: u32,
}

// AudioQueue is opaque — we only handle the pointer.
#[repr(C)]
struct OpaqueAudioQueue {
    _p: [u8; 0],
}
type AudioQueueRef = *mut OpaqueAudioQueue;

/// AudioQueueBuffer — CoreAudio hands us one of these in the callback.
/// Layout matches `<AudioToolbox/AudioQueue.h>` exactly up to the
/// fields we touch; the tail (packet descriptions) is fine to ignore
/// because we never give it any.
#[repr(C)]
struct AudioQueueBuffer {
    mAudioDataBytesCapacity: u32,
    mAudioData: *mut c_void,
    mAudioDataByteSize: u32,
    mUserData: *mut c_void,
    // Remaining fields (PacketDescriptions) omitted — we never set them.
}
type AudioQueueBufferRef = *mut AudioQueueBuffer;

type AudioQueueOutputCallback = unsafe extern "C" fn(
    inUserData: *mut c_void,
    inAQ: AudioQueueRef,
    inBuffer: AudioQueueBufferRef,
);

type Fn_AudioQueueNewOutput = unsafe extern "C" fn(
    inFormat: *const AudioStreamBasicDescription,
    inCallbackProc: AudioQueueOutputCallback,
    inUserData: *mut c_void,
    inCallbackRunLoop: *mut c_void,
    inCallbackRunLoopMode: *const c_void,
    inFlags: u32,
    outAQ: *mut AudioQueueRef,
) -> OSStatus;

type Fn_AudioQueueAllocateBuffer = unsafe extern "C" fn(
    inAQ: AudioQueueRef,
    inBufferByteSize: u32,
    outBuffer: *mut AudioQueueBufferRef,
) -> OSStatus;

type Fn_AudioQueueEnqueueBuffer = unsafe extern "C" fn(
    inAQ: AudioQueueRef,
    inBuffer: AudioQueueBufferRef,
    inNumPacketDescs: u32,
    inPacketDescs: *const c_void,
) -> OSStatus;

type Fn_AudioQueueStart =
    unsafe extern "C" fn(inAQ: AudioQueueRef, inStartTime: *const c_void) -> OSStatus;

type Fn_AudioQueueStop = unsafe extern "C" fn(inAQ: AudioQueueRef, inImmediate: u8) -> OSStatus;

type Fn_AudioQueuePause = unsafe extern "C" fn(inAQ: AudioQueueRef) -> OSStatus;

type Fn_AudioQueueDispose = unsafe extern "C" fn(inAQ: AudioQueueRef, inImmediate: u8) -> OSStatus;

struct AtLib {
    _lib: Library,
    AudioQueueNewOutput: Fn_AudioQueueNewOutput,
    AudioQueueAllocateBuffer: Fn_AudioQueueAllocateBuffer,
    AudioQueueEnqueueBuffer: Fn_AudioQueueEnqueueBuffer,
    AudioQueueStart: Fn_AudioQueueStart,
    AudioQueueStop: Fn_AudioQueueStop,
    AudioQueuePause: Fn_AudioQueuePause,
    AudioQueueDispose: Fn_AudioQueueDispose,
}

unsafe impl Send for AtLib {}
unsafe impl Sync for AtLib {}

impl AtLib {
    fn load() -> Result<Arc<Self>> {
        // Try the framework path first (what CoreAudio-typed dylibs
        // usually sit at on macOS) then fall back to the short name
        // for environments that already have it on DYLD_LIBRARY_PATH.
        const CANDIDATES: &[&str] = &[
            "/System/Library/Frameworks/AudioToolbox.framework/AudioToolbox",
            "AudioToolbox.framework/AudioToolbox",
            "AudioToolbox",
        ];
        let mut last_err: Option<libloading::Error> = None;
        for path in CANDIDATES {
            match unsafe { Library::new(path) } {
                Ok(lib) => return Self::bind(lib),
                Err(e) => last_err = Some(e),
            }
        }
        // CANDIDATES is non-empty, so the loop always sets last_err.
        Err(Error::LibraryLoad {
            backend: "coreaudio",
            soname: "AudioToolbox",
            source: last_err.expect("CANDIDATES is non-empty"),
        })
    }

    fn bind(lib: Library) -> Result<Arc<Self>> {
        unsafe {
            macro_rules! sym {
                ($name:ident, $ty:ty) => {{
                    let s: Symbol<$ty> = lib
                        .get(concat!(stringify!($name), "\0").as_bytes())
                        .map_err(|e| Error::SymbolMissing {
                            backend: "coreaudio",
                            symbol: stringify!($name),
                            source: e,
                        })?;
                    *s
                }};
            }
            Ok(Arc::new(AtLib {
                AudioQueueNewOutput: sym!(AudioQueueNewOutput, Fn_AudioQueueNewOutput),
                AudioQueueAllocateBuffer: sym!(
                    AudioQueueAllocateBuffer,
                    Fn_AudioQueueAllocateBuffer
                ),
                AudioQueueEnqueueBuffer: sym!(AudioQueueEnqueueBuffer, Fn_AudioQueueEnqueueBuffer),
                AudioQueueStart: sym!(AudioQueueStart, Fn_AudioQueueStart),
                AudioQueueStop: sym!(AudioQueueStop, Fn_AudioQueueStop),
                AudioQueuePause: sym!(AudioQueuePause, Fn_AudioQueuePause),
                AudioQueueDispose: sym!(AudioQueueDispose, Fn_AudioQueueDispose),
                _lib: lib,
            }))
        }
    }
}

fn lib() -> Result<Arc<AtLib>> {
    static CACHED: OnceLock<Mutex<Option<Arc<AtLib>>>> = OnceLock::new();
    let slot = CACHED.get_or_init(|| Mutex::new(None));
    let mut g = slot.lock().unwrap();
    if let Some(l) = g.as_ref() {
        return Ok(l.clone());
    }
    let l = AtLib::load()?;
    *g = Some(l.clone());
    Ok(l)
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub(crate) struct CoreAudioBackend;

impl Backend for CoreAudioBackend {
    fn name(&self) -> &'static str {
        "coreaudio"
    }
    fn description(&self) -> &'static str {
        "CoreAudio (AudioToolbox.framework via libloading, AudioQueue output)"
    }

    fn probe(&self) -> Result<()> {
        let l = lib()?;
        unsafe {
            let asbd = f32_asbd(44_100.0, 2);
            let mut queue: AudioQueueRef = ptr::null_mut();
            // The callback is never invoked for a probe (we dispose
            // before Start), so a no-op trampoline suffices. Userdata
            // is null; the callback never reads it.
            let r = (l.AudioQueueNewOutput)(
                &asbd,
                probe_noop_cb,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                0,
                &mut queue,
            );
            if r != 0 || queue.is_null() {
                return Err(Error::DeviceOpen {
                    backend: "coreaudio",
                    detail: format!("AudioQueueNewOutput OSStatus={r}"),
                });
            }
            (l.AudioQueueDispose)(queue, 1);
        }
        Ok(())
    }

    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>> {
        let l = lib()?;
        unsafe { open_inner(l, req, cb) }
    }
}

unsafe extern "C" fn probe_noop_cb(_user: *mut c_void, _q: AudioQueueRef, _b: AudioQueueBufferRef) {
}

fn f32_asbd(rate: f64, channels: u16) -> AudioStreamBasicDescription {
    let ch = channels.max(1) as u32;
    AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 4 * ch,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4 * ch,
        mChannelsPerFrame: ch,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// Heap-owned state reachable from the CA thread via a raw pointer in
/// `inUserData`. Kept alive by the `Stream` handle; dropped when the
/// stream is dropped and `AudioQueueDispose` has returned (so we know
/// the CA thread won't call us again).
struct CallbackState {
    cb: Mutex<Callback>,
    channels: usize,
    paused: Arc<AtomicBool>,
    frames_played: Arc<AtomicU64>,
}

unsafe extern "C" fn output_trampoline(
    user: *mut c_void,
    _q: AudioQueueRef,
    buffer: AudioQueueBufferRef,
) {
    if user.is_null() || buffer.is_null() {
        return;
    }
    let state = &*(user as *const CallbackState);
    let buf = &mut *buffer;
    let capacity = buf.mAudioDataBytesCapacity as usize;
    let frames = capacity / (4 * state.channels);
    let samples = frames * state.channels;
    let slice = std::slice::from_raw_parts_mut(buf.mAudioData as *mut f32, samples);

    if state.paused.load(Ordering::Relaxed) {
        for s in slice.iter_mut() {
            *s = 0.0;
        }
    } else {
        let info = CallbackInfo {
            frames_played: state.frames_played.load(Ordering::Relaxed),
        };
        let mut g = state.cb.lock().unwrap();
        (g)(slice, &info);
    }

    buf.mAudioDataByteSize = (samples * 4) as u32;
    if !state.paused.load(Ordering::Relaxed) {
        state
            .frames_played
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    // Re-enqueue — without this, CA stops calling us after three
    // buffers (the ones we pre-allocated).
    if let Some(l) = LIB_FOR_CALLBACK.get() {
        let _ = (l.AudioQueueEnqueueBuffer)(_q, buffer, 0, ptr::null());
    }
}

/// AudioQueue's callback only gives us `inUserData`; `libloading`'s fn
/// pointers live in `AtLib`, which we'd ordinarily reach through the
/// state. Stashing the `Arc<AtLib>` once here lets the trampoline stay
/// `extern "C"` without widening the userdata pointer.
static LIB_FOR_CALLBACK: OnceLock<Arc<AtLib>> = OnceLock::new();

/// Number of AudioQueue buffers we keep in flight. Three is the
/// canonical value from Apple's own TechNote — two is too tight under
/// scheduling pressure, four adds unnecessary latency.
const NUM_BUFFERS: usize = 3;

unsafe fn open_inner(
    l: Arc<AtLib>,
    req: StreamRequest,
    cb: Callback,
) -> Result<Box<dyn StreamImpl>> {
    let _ = LIB_FOR_CALLBACK.set(l.clone()); // no-op on subsequent opens

    let channels = req.channels.clamp(1, 8);
    let asbd = f32_asbd(req.sample_rate as f64, channels);

    let paused = Arc::new(AtomicBool::new(false));
    let frames_played = Arc::new(AtomicU64::new(0));

    let state = Box::new(CallbackState {
        cb: Mutex::new(cb),
        channels: channels as usize,
        paused: paused.clone(),
        frames_played: frames_played.clone(),
    });
    let state_ptr = Box::into_raw(state);

    let mut queue: AudioQueueRef = ptr::null_mut();
    let r = (l.AudioQueueNewOutput)(
        &asbd,
        output_trampoline,
        state_ptr as *mut c_void,
        ptr::null_mut(),
        ptr::null(),
        0,
        &mut queue,
    );
    if r != 0 || queue.is_null() {
        drop(Box::from_raw(state_ptr));
        return Err(Error::DeviceOpen {
            backend: "coreaudio",
            detail: format!("AudioQueueNewOutput OSStatus={r}"),
        });
    }

    // `NUM_BUFFERS` buffers × ~20 ms each. The CA thread round-robins
    // through them as the user's callback refills them.
    let frames_per_buf = req
        .buffer_frames
        .map(|n| n as usize)
        .unwrap_or_else(|| (req.sample_rate as usize / 50).max(64));
    let bytes_per_buf = (frames_per_buf * channels as usize * 4) as u32;

    let mut buffers: [AudioQueueBufferRef; NUM_BUFFERS] = [ptr::null_mut(); NUM_BUFFERS];
    for slot in &mut buffers {
        let r =
            (l.AudioQueueAllocateBuffer)(queue, bytes_per_buf, slot as *mut AudioQueueBufferRef);
        if r != 0 || slot.is_null() {
            (l.AudioQueueDispose)(queue, 1);
            drop(Box::from_raw(state_ptr));
            return Err(Error::DeviceOpen {
                backend: "coreaudio",
                detail: format!("AudioQueueAllocateBuffer OSStatus={r}"),
            });
        }
        // Prime with silence and the full capacity so CA doesn't think
        // the buffer is under-filled.
        let buf = &mut **slot;
        buf.mAudioDataByteSize = bytes_per_buf;
        ptr::write_bytes(buf.mAudioData as *mut u8, 0, bytes_per_buf as usize);
        (l.AudioQueueEnqueueBuffer)(queue, *slot, 0, ptr::null());
    }

    let r = (l.AudioQueueStart)(queue, ptr::null());
    if r != 0 {
        (l.AudioQueueDispose)(queue, 1);
        drop(Box::from_raw(state_ptr));
        return Err(Error::DeviceOpen {
            backend: "coreaudio",
            detail: format!("AudioQueueStart OSStatus={r}"),
        });
    }

    // Conservative software-side latency estimate: the number of
    // queued buffers times the per-buffer duration. Does NOT include
    // any hardware-side delay (Bluetooth, USB-DAC, HDMI routing) —
    // those would require loading CoreAudio.framework to query
    // `kAudioDevicePropertyLatency` on the audio device ID. That's
    // tracked as a follow-up; for now `latency()` is a floor, not a
    // ceiling, on BT sinks.
    let sw_latency_ns = ((NUM_BUFFERS * frames_per_buf) as u64).saturating_mul(1_000_000_000)
        / (req.sample_rate.max(1) as u64);

    Ok(Box::new(CoreAudioStream {
        lib: l,
        queue: QueuePtr(queue),
        state_ptr,
        paused,
        sw_latency_ns,
        format: StreamFormat {
            sample_rate: req.sample_rate,
            channels,
            format: SampleFormat::F32,
        },
        stopped: false,
    }))
}

#[derive(Copy, Clone)]
struct QueuePtr(AudioQueueRef);
unsafe impl Send for QueuePtr {}

struct CoreAudioStream {
    lib: Arc<AtLib>,
    queue: QueuePtr,
    /// Heap-owned callback state pointed at by the CA thread.
    state_ptr: *mut CallbackState,
    paused: Arc<AtomicBool>,
    /// Software-side latency estimate in nanoseconds (see computation
    /// in `open_inner`). Does not include hardware/BT-side delay.
    sw_latency_ns: u64,
    format: StreamFormat,
    stopped: bool,
}

unsafe impl Send for CoreAudioStream {}

impl StreamImpl for CoreAudioStream {
    fn play(&mut self) -> Result<()> {
        self.paused.store(false, Ordering::Relaxed);
        unsafe {
            let _ = (self.lib.AudioQueueStart)(self.queue.0, ptr::null());
        }
        Ok(())
    }
    fn pause(&mut self) -> Result<()> {
        self.paused.store(true, Ordering::Relaxed);
        unsafe {
            let _ = (self.lib.AudioQueuePause)(self.queue.0);
        }
        Ok(())
    }
    fn format(&self) -> StreamFormat {
        self.format
    }
    fn latency(&self) -> Option<Duration> {
        // Software-side estimate only — see the note in `open_inner`.
        // A follow-up should reach through `kAudioDevicePropertyLatency`
        // on the queue's current device to cover Bluetooth sinks.
        Some(Duration::from_nanos(self.sw_latency_ns))
    }
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        unsafe {
            // inImmediate = 1 (true) — stop + flush and guarantee the
            // callback won't fire again after this returns.
            let _ = (self.lib.AudioQueueStop)(self.queue.0, 1);
            let _ = (self.lib.AudioQueueDispose)(self.queue.0, 1);
            // Now safe to drop the Box the trampoline was reading.
            drop(Box::from_raw(self.state_ptr));
        }
    }
}
