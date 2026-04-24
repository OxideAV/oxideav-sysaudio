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
//!
//! For `latency()` we *also* dlopen `CoreAudio.framework` (the HAL
//! dylib underneath AudioToolbox) so we can reach
//! `AudioObjectGetPropertyData` and query the real hardware-side
//! delay on the device the queue is currently bound to — critical
//! for BT sinks, USB DACs and HDMI, where the AudioQueue's buffer
//! depth is tiny compared to the total pipeline. If CoreAudio.framework
//! fails to load (shouldn't on any modern macOS, but we defend
//! against it) we silently fall back to the software-only floor.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
// FourCC selectors and `kAudio…` constants are spelled exactly as
// Apple's headers publish them, for grep-ability against the HAL /
// AudioQueue docs.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
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
        for &path in CANDIDATES {
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
// CoreAudio.framework (HAL) — loaded separately for hardware latency.
// ---------------------------------------------------------------------------

/// AudioDeviceID / AudioObjectID / AudioStreamID — all just UInt32 in
/// Apple's HAL.
type AudioObjectID = u32;

/// `AudioObjectPropertyAddress` — 12 bytes; the (selector, scope, element)
/// triple every `AudioObjectGetPropertyData*` call takes.
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioObjectPropertyAddress {
    mSelector: u32,
    mScope: u32,
    mElement: u32,
}

/// FourCC helper — build a big-endian packed u32 the same way
/// `kAudioFormatLinearPCM` above does.
const fn four_cc(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*b)
}

// HAL "root" object + property to reach the current default output.
// kAudioObjectSystemObject is the documented singleton ID 1.
const kAudioObjectSystemObject: u32 = 1;
const kAudioHardwarePropertyDefaultOutputDevice: u32 = four_cc(b"dOut");

// Scope/element selectors on the HAL object tree.
const kAudioObjectPropertyScopeGlobal: u32 = four_cc(b"glob");
const kAudioObjectPropertyScopeOutput: u32 = four_cc(b"outp");
// Element 0 is "main" (formerly "master"); either name addresses the
// same element on every macOS version we care about.
const kAudioObjectPropertyElementMain: u32 = 0;

// Device-side hardware latency selectors.
const kAudioDevicePropertyLatency: u32 = four_cc(b"ltnc");
const kAudioDevicePropertyBufferFrameSize: u32 = four_cc(b"fsiz");
const kAudioDevicePropertySafetyOffset: u32 = four_cc(b"saft");
// Stream enumeration + per-stream latency (scope = output).
const kAudioDevicePropertyStreams: u32 = four_cc(b"stm#");
const kAudioStreamPropertyLatency: u32 = four_cc(b"ltnc");

/// `AudioObjectGetPropertyData(inObjectID, inAddress, 0, NULL,
/// ioDataSize, outData)` — the getter every HAL query funnels
/// through. The two `UInt32` args after the address are the
/// "qualifier" pair (we always pass (0, NULL)).
type Fn_AudioObjectGetPropertyData = unsafe extern "C" fn(
    inObjectID: AudioObjectID,
    inAddress: *const AudioObjectPropertyAddress,
    inQualifierDataSize: u32,
    inQualifierData: *const c_void,
    ioDataSize: *mut u32,
    outData: *mut c_void,
) -> OSStatus;

struct CaLib {
    _lib: Library,
    AudioObjectGetPropertyData: Fn_AudioObjectGetPropertyData,
}

unsafe impl Send for CaLib {}
unsafe impl Sync for CaLib {}

impl CaLib {
    fn load() -> std::result::Result<Arc<Self>, libloading::Error> {
        const CANDIDATES: &[&str] = &[
            "/System/Library/Frameworks/CoreAudio.framework/CoreAudio",
            "CoreAudio.framework/CoreAudio",
            "CoreAudio",
        ];
        let mut last_err: Option<libloading::Error> = None;
        for &path in CANDIDATES {
            match unsafe { Library::new(path) } {
                Ok(lib) => return Self::bind(lib),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("CANDIDATES is non-empty"))
    }

    fn bind(lib: Library) -> std::result::Result<Arc<Self>, libloading::Error> {
        unsafe {
            let s: Symbol<Fn_AudioObjectGetPropertyData> =
                lib.get(b"AudioObjectGetPropertyData\0")?;
            let f = *s;
            Ok(Arc::new(CaLib {
                AudioObjectGetPropertyData: f,
                _lib: lib,
            }))
        }
    }
}

/// Load (and cache) `CoreAudio.framework`. Unlike `lib()`, this never
/// returns `Err` to the caller — it maps a load failure into `None` so
/// `latency()` can degrade to the software floor without poisoning
/// `open()`.
fn ca_lib() -> Option<Arc<CaLib>> {
    static CACHED: OnceLock<Mutex<Option<Option<Arc<CaLib>>>>> = OnceLock::new();
    let slot = CACHED.get_or_init(|| Mutex::new(None));
    let mut g = slot.lock().unwrap();
    if let Some(slot) = g.as_ref() {
        return slot.clone();
    }
    let loaded = CaLib::load().ok();
    *g = Some(loaded.clone());
    loaded
}

/// Query `prop` from `object` as a single `u32`. Returns `None` if the
/// property is missing, the wrong size, or the getter fails.
unsafe fn hal_get_u32(ca: &CaLib, object: AudioObjectID, selector: u32, scope: u32) -> Option<u32> {
    let addr = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let r = (ca.AudioObjectGetPropertyData)(
        object,
        &addr,
        0,
        ptr::null(),
        &mut size,
        &mut value as *mut u32 as *mut c_void,
    );
    if r == 0 && size as usize == std::mem::size_of::<u32>() {
        Some(value)
    } else {
        None
    }
}

/// Query the first output stream on `device` (if any). CoreAudio
/// exposes streams through `kAudioDevicePropertyStreams` on the output
/// scope; we only need the first one's latency — multi-stream output
/// devices are rare for playback and all streams on the same physical
/// device share the same HAL latency anyway.
unsafe fn hal_first_output_stream(ca: &CaLib, device: AudioObjectID) -> Option<AudioObjectID> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMain,
    };
    // Two-step idiom: first call with outData=NULL to learn the byte
    // size, then allocate and re-query. The HAL insists on this even
    // when we only want one element.
    let mut size: u32 = 0;
    // Apple's API treats "get size" specially: pass NULL outData and
    // a 0 ioDataSize-in / size-out receives the real size. Several
    // HAL impls tolerate a non-NULL outData too as long as
    // *ioDataSize == 0, but NULL is the documented form.
    let r =
        (ca.AudioObjectGetPropertyData)(device, &addr, 0, ptr::null(), &mut size, ptr::null_mut());
    if r != 0 || size < std::mem::size_of::<AudioObjectID>() as u32 {
        return None;
    }
    let count = size as usize / std::mem::size_of::<AudioObjectID>();
    let mut buf: Vec<AudioObjectID> = vec![0; count];
    let r = (ca.AudioObjectGetPropertyData)(
        device,
        &addr,
        0,
        ptr::null(),
        &mut size,
        buf.as_mut_ptr() as *mut c_void,
    );
    if r != 0 {
        return None;
    }
    buf.into_iter().next().filter(|id| *id != 0)
}

/// Total hardware-side latency (in frames) for the current output
/// device. Sums `device_latency`, `buffer_frame_size`, `safety_offset`
/// and `stream_latency` per the HAL reference. Returns `None` if we
/// can't resolve a device to query, in which case callers should fall
/// back to the software-only floor.
///
/// **Device resolution**: Apple's docs state that
/// `kAudioQueueProperty_CurrentDevice` has value type `CFStringRef`
/// (a device UID), not `AudioDeviceID` — decoding that would drag in
/// CoreFoundation bindings this crate has deliberately avoided. The
/// AudioQueue we build never has `AudioQueueSetProperty` called on
/// it, so it always follows the system default output. We therefore
/// resolve via `kAudioHardwarePropertyDefaultOutputDevice` on the HAL
/// root, which yields the same numeric `AudioDeviceID` the queue is
/// using. This is already the shape Chromium/Firefox/etc. end up at
/// after doing the UID→DeviceID dance; we just skip the middle.
unsafe fn query_hardware_latency_frames(ca: &CaLib) -> Option<u32> {
    let default_addr = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut device: AudioObjectID = 0;
    let mut size: u32 = std::mem::size_of::<AudioObjectID>() as u32;
    let r = (ca.AudioObjectGetPropertyData)(
        kAudioObjectSystemObject,
        &default_addr,
        0,
        ptr::null(),
        &mut size,
        &mut device as *mut AudioObjectID as *mut c_void,
    );
    if r != 0 || device == 0 {
        return None;
    }

    // Device-side fixed latency + live buffer size + safety offset on
    // the OUTPUT scope. Any of these may be unreported on exotic
    // hardware; treat a missing property as 0 rather than bailing out
    // so BT headphones (which report their 100ms-ish latency through
    // `kAudioDevicePropertyLatency`) still get counted even if e.g.
    // their buffer-frame-size query fails.
    let dev_latency = hal_get_u32(
        ca,
        device,
        kAudioDevicePropertyLatency,
        kAudioObjectPropertyScopeOutput,
    )
    .or_else(|| {
        hal_get_u32(
            ca,
            device,
            kAudioDevicePropertyLatency,
            kAudioObjectPropertyScopeGlobal,
        )
    })
    .unwrap_or(0);
    let buffer_frames = hal_get_u32(
        ca,
        device,
        kAudioDevicePropertyBufferFrameSize,
        kAudioObjectPropertyScopeOutput,
    )
    .or_else(|| {
        hal_get_u32(
            ca,
            device,
            kAudioDevicePropertyBufferFrameSize,
            kAudioObjectPropertyScopeGlobal,
        )
    })
    .unwrap_or(0);
    let safety_offset = hal_get_u32(
        ca,
        device,
        kAudioDevicePropertySafetyOffset,
        kAudioObjectPropertyScopeOutput,
    )
    .unwrap_or(0);

    // Per-stream latency on the device's first output stream. Optional
    // — not all HAL drivers populate it, and it's typically 0-few
    // frames (format-conversion buffers) on top of the device figure.
    let stream_latency = hal_first_output_stream(ca, device)
        .and_then(|stream| {
            hal_get_u32(
                ca,
                stream,
                kAudioStreamPropertyLatency,
                kAudioObjectPropertyScopeGlobal,
            )
        })
        .unwrap_or(0);

    Some(
        dev_latency
            .saturating_add(buffer_frames)
            .saturating_add(safety_offset)
            .saturating_add(stream_latency),
    )
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

    // Software-side floor: three queued buffers × per-buffer duration.
    // This is a hard lower bound on how long it takes a sample the
    // caller hands us to reach the device. The hardware-side component
    // (BT radio delay, USB-DAC pipeline depth, HDMI routing, HAL
    // safety offset) comes from `query_hardware_latency_frames` at
    // `latency()` time — done live because the bound device, and thus
    // the hardware delay, can change after `open()` (default-output
    // switch, hot-plug).
    let sw_latency_ns = ((NUM_BUFFERS * frames_per_buf) as u64).saturating_mul(1_000_000_000)
        / (req.sample_rate.max(1) as u64);

    // Attempt to load CoreAudio.framework for HAL latency queries.
    // Graceful degradation: if it fails (shouldn't on any modern
    // macOS), `latency()` simply returns the software floor.
    let ca = ca_lib();

    Ok(Box::new(CoreAudioStream {
        lib: l,
        ca,
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
    /// CoreAudio.framework HAL bindings for hardware-latency queries.
    /// `None` if the framework failed to load (graceful-degradation
    /// path — `latency()` falls back to the software floor).
    ca: Option<Arc<CaLib>>,
    queue: QueuePtr,
    /// Heap-owned callback state pointed at by the CA thread.
    state_ptr: *mut CallbackState,
    paused: Arc<AtomicBool>,
    /// Software-side latency floor in nanoseconds — the `NUM_BUFFERS
    /// × frames_per_buf` component. See `open_inner`. The HAL-reported
    /// hardware component is added at `latency()`-call time.
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
        // Software floor (AudioQueue buffer depth) + hardware
        // component (HAL: device latency + buffer frame size +
        // safety offset + stream latency) converted through the
        // stream sample rate. If CoreAudio.framework didn't load or
        // the HAL query fails, we transparently fall back to the
        // software floor so the caller always gets *some* answer.
        let mut total_ns = self.sw_latency_ns;
        if let Some(ca) = self.ca.as_ref() {
            // SAFETY: `ca` owns its `Library` handle, so the fn
            // pointer stays mapped for the duration of the call.
            let hw_frames = unsafe { query_hardware_latency_frames(ca) };
            if let Some(frames) = hw_frames {
                let rate = self.format.sample_rate.max(1) as u64;
                let hw_ns = (frames as u64).saturating_mul(1_000_000_000) / rate;
                total_ns = total_ns.saturating_add(hw_ns);
            }
        }
        Some(Duration::from_nanos(total_ns))
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

// ---------------------------------------------------------------------------
// Tests — constant/layout checks only. The end-to-end HAL query path
// needs a real audio device and is covered by manual verification via
// `oxideplay` on macOS.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_constants_match_apple_headers() {
        // Every HAL FourCC should decode to the big-endian-packed
        // ASCII the Core Audio headers document. If any of these
        // regress, the HAL query returns garbage.
        assert_eq!(kAudioObjectSystemObject, 1);
        assert_eq!(
            kAudioHardwarePropertyDefaultOutputDevice,
            u32::from_be_bytes(*b"dOut")
        );
        assert_eq!(
            kAudioObjectPropertyScopeGlobal,
            u32::from_be_bytes(*b"glob")
        );
        assert_eq!(
            kAudioObjectPropertyScopeOutput,
            u32::from_be_bytes(*b"outp")
        );
        assert_eq!(kAudioObjectPropertyElementMain, 0);
        assert_eq!(kAudioDevicePropertyLatency, u32::from_be_bytes(*b"ltnc"));
        assert_eq!(
            kAudioDevicePropertyBufferFrameSize,
            u32::from_be_bytes(*b"fsiz")
        );
        assert_eq!(
            kAudioDevicePropertySafetyOffset,
            u32::from_be_bytes(*b"saft")
        );
        assert_eq!(kAudioDevicePropertyStreams, u32::from_be_bytes(*b"stm#"));
        assert_eq!(kAudioStreamPropertyLatency, u32::from_be_bytes(*b"ltnc"));
        // Cross-check against the existing `kAudioFormatLinearPCM`
        // literal to make sure our `four_cc` helper matches the
        // hand-written form used elsewhere in this file.
        assert_eq!(four_cc(b"lpcm"), kAudioFormatLinearPCM);
    }

    #[test]
    fn property_address_layout_is_3_u32s() {
        // Must match the C layout `<CoreAudio/AudioHardwareBase.h>`
        // publishes: three UInt32 fields, no padding, total 12 bytes.
        // Drift here would silently feed AudioObjectGetPropertyData
        // misaligned selectors.
        assert_eq!(std::mem::size_of::<AudioObjectPropertyAddress>(), 12);
        assert_eq!(std::mem::align_of::<AudioObjectPropertyAddress>(), 4);

        let a = AudioObjectPropertyAddress {
            mSelector: 0x1111_1111,
            mScope: 0x2222_2222,
            mElement: 0x3333_3333,
        };
        let base = &a as *const _ as usize;
        assert_eq!(&a.mSelector as *const _ as usize - base, 0);
        assert_eq!(&a.mScope as *const _ as usize - base, 4);
        assert_eq!(&a.mElement as *const _ as usize - base, 8);
    }
}
