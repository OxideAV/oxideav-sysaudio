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
use crate::format::{CallbackInfo, Device, SampleFormat, StreamFormat, StreamRequest};
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

/// `AudioQueueSetProperty(inAQ, inID, inData, inDataSize)`. Used to bind
/// the queue to a specific output device via
/// `kAudioQueueProperty_CurrentDevice` whose value is a `CFStringRef`
/// device UID — see `AudioQueue.h` line 271 in the macOS 26 SDK.
type Fn_AudioQueueSetProperty = unsafe extern "C" fn(
    inAQ: AudioQueueRef,
    inID: u32,
    inData: *const c_void,
    inDataSize: u32,
) -> OSStatus;

struct AtLib {
    _lib: Library,
    AudioQueueNewOutput: Fn_AudioQueueNewOutput,
    AudioQueueAllocateBuffer: Fn_AudioQueueAllocateBuffer,
    AudioQueueEnqueueBuffer: Fn_AudioQueueEnqueueBuffer,
    AudioQueueStart: Fn_AudioQueueStart,
    AudioQueueStop: Fn_AudioQueueStop,
    AudioQueuePause: Fn_AudioQueuePause,
    AudioQueueDispose: Fn_AudioQueueDispose,
    AudioQueueSetProperty: Fn_AudioQueueSetProperty,
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
                AudioQueueSetProperty: sym!(AudioQueueSetProperty, Fn_AudioQueueSetProperty),
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
// All audio devices known to the HAL (output + input + aggregate).
const kAudioHardwarePropertyDevices: u32 = four_cc(b"dev#");
// Per-device friendly name. The CFString-typed
// `kAudioObjectPropertyName` would drag in CoreFoundation, which this
// crate deliberately avoids; the deprecated `kAudioDevicePropertyDeviceName`
// returns a plain NUL-terminated C string buffer instead, so we use it
// for the label. Still present and populated on every shipping macOS.
const kAudioDevicePropertyDeviceName: u32 = four_cc(b"name");
// Per-device UID (CFStringRef) is the stable cross-boot id; decoding it
// needs CoreFoundation, so we expose the numeric AudioDeviceID as the
// `Device::id` token instead — opaque and stable within a boot session,
// which is all the contract promises.

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
// Persistent cross-boot device identifier (CFStringRef). AudioHardwareBase.h
// l. 734 in the macOS 26 SDK documents this as the `'uid '` selector whose
// value is a CFString — the exact shape `kAudioQueueProperty_CurrentDevice`
// wants on the AudioQueue side.
const kAudioDevicePropertyDeviceUID: u32 = four_cc(b"uid ");

// AudioQueue side: the property whose value is the CFStringRef device UID we
// just queried out of the HAL. `AudioQueue.h` l. 271 in the macOS 26 SDK
// publishes the selector as the FourCC `'aqcd'` (value type CFStringRef);
// `AudioQueueSetProperty(queue, kAudioQueueProperty_CurrentDevice, &cfstr,
// sizeof(CFStringRef))` reroutes the queue to that device. Called before
// `AudioQueueStart` so the queue never plays through the wrong endpoint.
const kAudioQueueProperty_CurrentDevice: u32 = four_cc(b"aqcd");

// Device-side sample-rate negotiation read-back. The HAL exposes the rate the
// device is currently running at (= the rate AudioQueue's mixer will
// resample into when our ASBD specifies a different one) as a single f64
// under `kAudioDevicePropertyNominalSampleRate` ('nsrt' per
// AudioHardwareBase.h). Reporting this through `preferred_format()` lets a
// caller resample once on their side and skip the queue's hidden conversion.
const kAudioDevicePropertyNominalSampleRate: u32 = four_cc(b"nsrt");

// Per-stream effective format on the device's first output stream.
// `AudioStream.h` documents 'sfmt' as an `AudioStreamBasicDescription` value
// — i.e. exactly the shape we already use for `AudioQueueNewOutput`. We read
// it to pick up the device's current channel count (the rate is also there
// but `NominalSampleRate` is the right answer on aggregate devices, which
// expose multiple streams).
const kAudioStreamPropertyVirtualFormat: u32 = four_cc(b"sfmt");

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

/// `AudioObjectGetPropertyDataSize(inObjectID, inAddress, 0, NULL,
/// outDataSize)` — the HAL's dedicated size query for variable-length
/// properties (device lists, stream lists, C-string names): the
/// documented way to learn a property's byte size before allocating.
/// Calling `AudioObjectGetPropertyData` with a NULL `outData` is NOT a
/// size query — on current macOS every such call fails (observed
/// empirically on 2026-07 hardware), which made device enumeration
/// silently return an empty list.
type Fn_AudioObjectGetPropertyDataSize = unsafe extern "C" fn(
    inObjectID: AudioObjectID,
    inAddress: *const AudioObjectPropertyAddress,
    inQualifierDataSize: u32,
    inQualifierData: *const c_void,
    outDataSize: *mut u32,
) -> OSStatus;

struct CaLib {
    _lib: Library,
    AudioObjectGetPropertyData: Fn_AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize: Fn_AudioObjectGetPropertyDataSize,
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
            let s_size: Symbol<Fn_AudioObjectGetPropertyDataSize> =
                lib.get(b"AudioObjectGetPropertyDataSize\0")?;
            let f_size = *s_size;
            Ok(Arc::new(CaLib {
                AudioObjectGetPropertyData: f,
                AudioObjectGetPropertyDataSize: f_size,
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

// ---------------------------------------------------------------------------
// CoreFoundation.framework — minimal surface for CFStringRef handling.
// ---------------------------------------------------------------------------
//
// `kAudioQueueProperty_CurrentDevice` wants a CFStringRef (the device UID)
// and `kAudioDevicePropertyDeviceUID` hands one back, the HAL retaining
// ownership over a CFString it allocated. We never need to read the string
// content — the CFStringRef is opaque from our side and we just thread the
// pointer from HAL → AudioQueueSetProperty → CFRelease. Only `CFRelease`
// is needed; we deliberately avoid `CFStringCreateWith*` / `CFStringGetCString`
// to keep the CoreFoundation footprint single-symbol.

#[repr(C)]
struct OpaqueCFType {
    _p: [u8; 0],
}
/// CFTypeRef / CFStringRef — both are opaque pointers from our POV.
type CFTypeRef = *const OpaqueCFType;

type Fn_CFRelease = unsafe extern "C" fn(cf: CFTypeRef);

struct CfLib {
    _lib: Library,
    CFRelease: Fn_CFRelease,
}

unsafe impl Send for CfLib {}
unsafe impl Sync for CfLib {}

impl CfLib {
    fn load() -> std::result::Result<Arc<Self>, libloading::Error> {
        const CANDIDATES: &[&str] = &[
            "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation",
            "CoreFoundation.framework/CoreFoundation",
            "CoreFoundation",
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
            let s: Symbol<Fn_CFRelease> = lib.get(b"CFRelease\0")?;
            let f = *s;
            Ok(Arc::new(CfLib {
                CFRelease: f,
                _lib: lib,
            }))
        }
    }
}

/// Load (and cache) `CoreFoundation.framework`. Like `ca_lib`, a failure to
/// load surfaces as `None` rather than poisoning `open()` — per-device
/// routing is degraded to an `UnsupportedFormat` error in that case so the
/// caller can either retry against the default endpoint or surface the
/// platform-misconfigured state.
fn cf_lib() -> Option<Arc<CfLib>> {
    static CACHED: OnceLock<Mutex<Option<Option<Arc<CfLib>>>>> = OnceLock::new();
    let slot = CACHED.get_or_init(|| Mutex::new(None));
    let mut g = slot.lock().unwrap();
    if let Some(slot) = g.as_ref() {
        return slot.clone();
    }
    let loaded = CfLib::load().ok();
    *g = Some(loaded.clone());
    loaded
}

/// Query `kAudioDevicePropertyDeviceUID` for `device` and return the raw
/// CFStringRef the HAL handed back. The caller owns the reference (Apple's
/// "Get/Copy" rule: any "Copy" / "Create" — and the docs explicitly state
/// the caller is responsible for releasing the returned CFObject) and
/// must `CFRelease` it once the AudioQueue has consumed it. Returns
/// `None` if the property is unavailable or the size is wrong.
unsafe fn hal_get_device_uid(ca: &CaLib, device: AudioObjectID) -> Option<CFTypeRef> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceUID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value: CFTypeRef = ptr::null();
    let mut size: u32 = std::mem::size_of::<CFTypeRef>() as u32;
    let r = (ca.AudioObjectGetPropertyData)(
        device,
        &addr,
        0,
        ptr::null(),
        &mut size,
        &mut value as *mut CFTypeRef as *mut c_void,
    );
    if r == 0 && !value.is_null() && size as usize == std::mem::size_of::<CFTypeRef>() {
        Some(value)
    } else {
        None
    }
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
    // Two-step idiom: `AudioObjectGetPropertyDataSize` to learn the
    // byte size, then allocate and query the data. The HAL insists on
    // this even when we only want one element.
    let mut size: u32 = 0;
    let r = (ca.AudioObjectGetPropertyDataSize)(device, &addr, 0, ptr::null(), &mut size);
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

/// Total hardware-side latency (in frames) for an output device. Sums
/// `device_latency`, `buffer_frame_size`, `safety_offset` and
/// `stream_latency` per the HAL reference. Returns `None` if we can't
/// resolve a device to query, in which case callers should fall back to
/// the software-only floor.
///
/// **Device resolution**: when `bound_device` is `Some(id)` (the queue
/// was explicitly routed via `kAudioQueueProperty_CurrentDevice`), the
/// HAL is queried on that id directly so latency stays correct after
/// per-device routing. When `None`, the queue follows the system
/// default and we resolve `kAudioHardwarePropertyDefaultOutputDevice`
/// at call time — which keeps the figure live across user-driven
/// default-output switches.
unsafe fn query_hardware_latency_frames(
    ca: &CaLib,
    bound_device: Option<AudioObjectID>,
) -> Option<u32> {
    let device = match bound_device {
        Some(d) if d != 0 => d,
        _ => {
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
            device
        }
    };

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

/// All device IDs the HAL knows about (`kAudioHardwarePropertyDevices`
/// on the system object). Same two-step size-then-data idiom as
/// `hal_first_output_stream`.
unsafe fn hal_all_devices(ca: &CaLib) -> Vec<AudioObjectID> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = 0;
    let r = (ca.AudioObjectGetPropertyDataSize)(
        kAudioObjectSystemObject,
        &addr,
        0,
        ptr::null(),
        &mut size,
    );
    if r != 0 || size < std::mem::size_of::<AudioObjectID>() as u32 {
        return Vec::new();
    }
    let count = size as usize / std::mem::size_of::<AudioObjectID>();
    let mut buf: Vec<AudioObjectID> = vec![0; count];
    let r = (ca.AudioObjectGetPropertyData)(
        kAudioObjectSystemObject,
        &addr,
        0,
        ptr::null(),
        &mut size,
        buf.as_mut_ptr() as *mut c_void,
    );
    if r != 0 {
        return Vec::new();
    }
    buf.retain(|&id| id != 0);
    buf
}

/// `true` when `device` has at least one stream on the output scope —
/// i.e. it can play audio. Pure-input devices (built-in mic, USB
/// capture) report zero output streams and are dropped from the
/// playback list.
unsafe fn hal_is_output_device(ca: &CaLib, device: AudioObjectID) -> bool {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = 0;
    let r = (ca.AudioObjectGetPropertyDataSize)(device, &addr, 0, ptr::null(), &mut size);
    r == 0 && size >= std::mem::size_of::<AudioObjectID>() as u32
}

/// Friendly name for `device` via the deprecated-but-CFString-free
/// `kAudioDevicePropertyDeviceName`, which writes a NUL-terminated C
/// string into a caller buffer. Returns an empty string if the HAL
/// reports no name.
unsafe fn hal_device_name(ca: &CaLib, device: AudioObjectID) -> String {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceName,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = 0;
    let r = (ca.AudioObjectGetPropertyDataSize)(device, &addr, 0, ptr::null(), &mut size);
    if r != 0 || size == 0 {
        return String::new();
    }
    // Cap the buffer defensively — a sane device name is well under 1 KiB.
    let cap = (size as usize).min(4096);
    let mut buf: Vec<u8> = vec![0u8; cap];
    let mut got = cap as u32;
    let r = (ca.AudioObjectGetPropertyData)(
        device,
        &addr,
        0,
        ptr::null(),
        &mut got,
        buf.as_mut_ptr() as *mut c_void,
    );
    if r != 0 {
        return String::new();
    }
    // The buffer is a C string; trim at the first NUL.
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Query `selector` from `object` as a single `f64`. Returns `None` if the
/// property is missing, the wrong size, or the getter fails. Used for
/// `kAudioDevicePropertyNominalSampleRate`, whose value is a HAL-typed
/// `Float64`.
unsafe fn hal_get_f64(ca: &CaLib, object: AudioObjectID, selector: u32, scope: u32) -> Option<f64> {
    let addr = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value: f64 = 0.0;
    let mut size: u32 = std::mem::size_of::<f64>() as u32;
    let r = (ca.AudioObjectGetPropertyData)(
        object,
        &addr,
        0,
        ptr::null(),
        &mut size,
        &mut value as *mut f64 as *mut c_void,
    );
    if r == 0 && size as usize == std::mem::size_of::<f64>() {
        Some(value)
    } else {
        None
    }
}

/// Query `kAudioStreamPropertyVirtualFormat` for `stream` as an
/// `AudioStreamBasicDescription`. The HAL writes the stream's current
/// effective format — what AudioQueue would feed without invoking its
/// own rate / channel conversion. Returns `None` if the property is
/// missing, the wrong size, or the getter fails.
unsafe fn hal_get_asbd(ca: &CaLib, stream: AudioObjectID) -> Option<AudioStreamBasicDescription> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioStreamPropertyVirtualFormat,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value = AudioStreamBasicDescription::default();
    let mut size: u32 = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let r = (ca.AudioObjectGetPropertyData)(
        stream,
        &addr,
        0,
        ptr::null(),
        &mut size,
        &mut value as *mut AudioStreamBasicDescription as *mut c_void,
    );
    if r == 0 && size as usize == std::mem::size_of::<AudioStreamBasicDescription>() {
        Some(value)
    } else {
        None
    }
}

/// Numeric AudioDeviceID of the current default output, or 0 if the
/// query fails.
unsafe fn hal_default_output_device(ca: &CaLib) -> AudioObjectID {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut device: AudioObjectID = 0;
    let mut size: u32 = std::mem::size_of::<AudioObjectID>() as u32;
    let r = (ca.AudioObjectGetPropertyData)(
        kAudioObjectSystemObject,
        &addr,
        0,
        ptr::null(),
        &mut size,
        &mut device as *mut AudioObjectID as *mut c_void,
    );
    if r != 0 {
        0
    } else {
        device
    }
}

/// Enumerate output-capable devices via the HAL. Walks
/// `kAudioHardwarePropertyDevices`, keeps the ones with output streams,
/// labels each with its name, and tags the system default.
fn enumerate_output_devices() -> Result<Vec<Device>> {
    let ca = ca_lib().ok_or(Error::NotImplemented("coreaudio"))?;
    let mut out = Vec::new();
    unsafe {
        let default = hal_default_output_device(&ca);
        for id in hal_all_devices(&ca) {
            if !hal_is_output_device(&ca, id) {
                continue;
            }
            out.push(Device {
                id: id.to_string(),
                name: hal_device_name(&ca, id),
                is_default: id == default && default != 0,
            });
        }
    }
    Ok(out)
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

    fn output_devices(&self) -> Result<Vec<Device>> {
        enumerate_output_devices()
    }

    /// Direct-query override of the trait's enumerate-and-filter
    /// default: one HAL property read
    /// (`kAudioHardwarePropertyDefaultOutputDevice` on the system
    /// object) plus one name lookup, instead of walking every device
    /// and probing each for output streams. Must stay in agreement
    /// with the `is_default` tag `output_devices()` computes — the
    /// crate-level `default_output_device_matches_enumeration` test
    /// cross-checks the two paths on real hardware.
    fn default_output_device(&self) -> Result<Option<Device>> {
        let ca = ca_lib().ok_or(Error::NotImplemented("coreaudio"))?;
        unsafe {
            let id = hal_default_output_device(&ca);
            if id == 0 {
                // The HAL reports no default endpoint — a transient
                // state between hotplug events, or a machine with no
                // output hardware at all.
                return Ok(None);
            }
            Ok(Some(Device {
                id: id.to_string(),
                name: hal_device_name(&ca, id),
                is_default: true,
            }))
        }
    }

    /// Best-effort report of the device's `NominalSampleRate` + its first
    /// output stream's `VirtualFormat` channel count. The HAL returns the
    /// rate the device is currently running at, which is what AudioQueue's
    /// mixer would resample into when our ASBD specifies a different one
    /// — so reporting it back lets the caller resample once on their side
    /// and skip the queue's hidden conversion. `channels` is read from the
    /// virtual format of the device's first output stream; rate from
    /// `NominalSampleRate` on the device so aggregate devices (which expose
    /// per-stream rates that may disagree) are reported coherently.
    fn preferred_format(&self, device_id: Option<&str>) -> Result<StreamFormat> {
        let ca = ca_lib().ok_or(Error::NotImplemented("coreaudio"))?;
        unsafe {
            let device: AudioObjectID = match device_id {
                None => {
                    let d = hal_default_output_device(&ca);
                    if d == 0 {
                        return Err(Error::DeviceOpen {
                            backend: "coreaudio",
                            detail: "kAudioHardwarePropertyDefaultOutputDevice query failed".into(),
                        });
                    }
                    d
                }
                Some(id_str) => id_str.parse().map_err(|_| Error::UnsupportedFormat {
                    backend: "coreaudio",
                    detail: format!(
                        "device id {id_str:?} is not a numeric AudioDeviceID (must come from \
                         output_devices() on the same coreaudio backend)"
                    ),
                })?,
            };
            let rate = hal_get_f64(
                &ca,
                device,
                kAudioDevicePropertyNominalSampleRate,
                kAudioObjectPropertyScopeOutput,
            )
            .or_else(|| {
                hal_get_f64(
                    &ca,
                    device,
                    kAudioDevicePropertyNominalSampleRate,
                    kAudioObjectPropertyScopeGlobal,
                )
            })
            .ok_or(Error::UnsupportedFormat {
                backend: "coreaudio",
                detail: format!(
                    "HAL has no NominalSampleRate for AudioDeviceID {device} — not an output \
                     device or query refused"
                ),
            })?;
            // Round to the nearest u32: HAL rates are advertised as exact
            // decimals (44_100.0, 48_000.0, 96_000.0…) but the f64 path
            // means we should still tolerate a ULP-off reading rather than
            // truncate 47_999.9999 down to 47_999.
            let sample_rate = rate.round().max(0.0).min(u32::MAX as f64) as u32;
            let channels = hal_first_output_stream(&ca, device)
                .and_then(|s| hal_get_asbd(&ca, s))
                .map(|asbd| asbd.mChannelsPerFrame as u16)
                .filter(|c| *c >= 1)
                .unwrap_or(2);
            Ok(StreamFormat {
                sample_rate,
                channels,
                format: SampleFormat::F32,
            })
        }
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
    // Per-device routing on AudioQueue takes a CFStringRef device UID
    // (`kAudioQueueProperty_CurrentDevice`), whereas the opaque `Device::id`
    // we expose is the numeric `AudioDeviceID`. The HAL bridges between the
    // two: query `kAudioDevicePropertyDeviceUID` on the numeric id, hand the
    // resulting CFStringRef straight to `AudioQueueSetProperty`, then
    // `CFRelease` it once the queue has copied the value. CoreFoundation is
    // dlopen'd through `cf_lib()` so the no-link-time-deps invariant holds.
    //
    // If the caller asked for a device but the platform fails any required
    // step (CF can't load, the id isn't a decimal AudioDeviceID, the device
    // has no UID, or the AudioQueue refuses the property), surface a clean
    // `UnsupportedFormat` rather than silently routing to the system default —
    // that would defeat the entire purpose of `open_on` / `with_device`.
    let routing: Option<(AudioObjectID, Arc<CfLib>, Arc<CaLib>)> = match req.device.as_deref() {
        None => None,
        Some(id_str) => {
            let device_id: AudioObjectID =
                id_str.parse().map_err(|_| Error::UnsupportedFormat {
                    backend: "coreaudio",
                    detail: format!(
                        "device id {id_str:?} is not a numeric AudioDeviceID (must come from \
                         output_devices() on the same coreaudio backend)"
                    ),
                })?;
            let ca = ca_lib().ok_or(Error::UnsupportedFormat {
                backend: "coreaudio",
                detail: "CoreAudio.framework HAL is required for per-device routing but failed \
                         to load"
                    .into(),
            })?;
            let cf = cf_lib().ok_or(Error::UnsupportedFormat {
                backend: "coreaudio",
                detail: "CoreFoundation.framework is required for per-device routing but failed \
                         to load"
                    .into(),
            })?;
            Some((device_id, cf, ca))
        }
    };

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

    // Per-device routing: now that the queue exists, bind it to the
    // caller-requested endpoint before any buffer is enqueued or
    // `AudioQueueStart` runs. Per `AudioQueue.h` the property must be set
    // before the queue is started; setting it on a running queue is
    // ill-defined and typically returns `kAudioQueueErr_CannotStart`.
    if let Some((device_id, cf, ca)) = routing.as_ref() {
        let uid = match hal_get_device_uid(ca, *device_id) {
            Some(u) => u,
            None => {
                (l.AudioQueueDispose)(queue, 1);
                drop(Box::from_raw(state_ptr));
                return Err(Error::UnsupportedFormat {
                    backend: "coreaudio",
                    detail: format!(
                        "AudioDeviceID {device_id} has no DeviceUID property — not an \
                         output device or HAL refused the query"
                    ),
                });
            }
        };
        // AudioQueueSetProperty takes a pointer-to-CFStringRef (the property
        // value is CFStringRef, so the inData pointer is `&CFStringRef` and
        // inDataSize is `sizeof(CFStringRef)`).
        let r = (l.AudioQueueSetProperty)(
            queue,
            kAudioQueueProperty_CurrentDevice,
            &uid as *const CFTypeRef as *const c_void,
            std::mem::size_of::<CFTypeRef>() as u32,
        );
        // The queue retained the CFString internally (or refused, in which
        // case there's nothing to undo on the queue side). Either way the
        // HAL handed us a +1 ref to drop — Apple's get/copy rule: the
        // `kAudioDevicePropertyDeviceUID` docs say "the caller is
        // responsible for releasing the returned CFObject".
        (cf.CFRelease)(uid);
        if r != 0 {
            (l.AudioQueueDispose)(queue, 1);
            drop(Box::from_raw(state_ptr));
            return Err(Error::DeviceOpen {
                backend: "coreaudio",
                detail: format!("AudioQueueSetProperty(CurrentDevice={device_id}) OSStatus={r}"),
            });
        }
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

    // If we routed the queue at a specific device, lock latency queries
    // onto that device; otherwise the queue follows the system default
    // and `query_hardware_latency_frames` resolves the default at call
    // time (the historical behaviour).
    let bound_device = routing.as_ref().map(|(id, _, _)| *id);

    Ok(Box::new(CoreAudioStream {
        lib: l,
        ca,
        queue: QueuePtr(queue),
        state_ptr,
        paused,
        sw_latency_ns,
        bound_device,
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
    /// The AudioDeviceID this queue was routed to via
    /// `kAudioQueueProperty_CurrentDevice`, or `None` if the queue is
    /// following the system default endpoint. `latency()` queries the HAL
    /// against this id when set, so the figure stays correct for streams
    /// pinned to a non-default device (e.g. opened through `open_on`
    /// against a USB DAC).
    bound_device: Option<AudioObjectID>,
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
            let hw_frames = unsafe { query_hardware_latency_frames(ca, self.bound_device) };
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
        // Device-enumeration selectors.
        assert_eq!(kAudioHardwarePropertyDevices, u32::from_be_bytes(*b"dev#"));
        assert_eq!(kAudioDevicePropertyDeviceName, u32::from_be_bytes(*b"name"));
        // Per-device routing selectors (AudioHardwareBase.h l. 734 +
        // AudioQueue.h l. 271 in the macOS 26 SDK).
        assert_eq!(kAudioDevicePropertyDeviceUID, u32::from_be_bytes(*b"uid "));
        assert_eq!(
            kAudioQueueProperty_CurrentDevice,
            u32::from_be_bytes(*b"aqcd")
        );
        // Cross-check against the existing `kAudioFormatLinearPCM`
        // literal to make sure our `four_cc` helper matches the
        // hand-written form used elsewhere in this file.
        assert_eq!(four_cc(b"lpcm"), kAudioFormatLinearPCM);
    }

    #[test]
    fn non_numeric_device_id_returns_unsupported_format() {
        // The CoreAudio `Device::id` is the decimal `AudioDeviceID`. A caller
        // that hands us an opaque blob from a different backend (or otherwise
        // fabricates one) must NOT silently fall back to the system default —
        // they explicitly asked for a non-default device, so we surface
        // `UnsupportedFormat` from the parse step. This test exercises the
        // routing precondition without needing CoreAudio.framework to load
        // (it short-circuits at the str::parse step before any HAL call).
        use crate::format::StreamRequest;
        let backend = CoreAudioBackend;
        let req = StreamRequest::new(48_000, 2).with_device("not-a-decimal-id");
        let r = backend.open(req, Box::new(|_, _| {}));
        match r {
            Err(crate::Error::UnsupportedFormat { backend, detail }) => {
                assert_eq!(backend, "coreaudio");
                assert!(
                    detail.contains("not a numeric AudioDeviceID"),
                    "expected parse-failure message, got: {detail}"
                );
            }
            // AudioToolbox.framework couldn't be loaded — the routing check
            // wraps the AtLib load, so a `LibraryLoad` error is also acceptable
            // on hosts where the framework isn't available (which shouldn't be
            // any real macOS, but covers cross-target builds).
            Err(crate::Error::LibraryLoad { .. }) => {}
            Err(other) => {
                panic!("expected UnsupportedFormat for fabricated id, got error variant: {other}")
            }
            Ok(_) => panic!("CoreAudio accepted a non-numeric device id; routing bug"),
        }
    }

    #[test]
    fn enumeration_includes_the_default_output_device() {
        // Regression test for the empty-enumeration bug: every
        // variable-length HAL query used `AudioObjectGetPropertyData`
        // with a NULL outData as a "size query", which fails on
        // current macOS — so `hal_all_devices` returned an empty Vec
        // and `output_devices()` reported zero devices on hosts with
        // working speakers, while `open()`/`latency()` (fixed-size
        // queries) kept working and masked it. The real size query is
        // the dedicated `AudioObjectGetPropertyDataSize` symbol.
        //
        // Gate: only meaningful when the HAL reports a default output
        // device. A headless CI box (or a non-mac cross-build) skips
        // cleanly.
        let Some(ca) = ca_lib() else {
            eprintln!("SKIP: CoreAudio.framework not loadable");
            return;
        };
        let default = unsafe { hal_default_output_device(&ca) };
        if default == 0 {
            eprintln!("SKIP: HAL reports no default output device");
            return;
        }
        let devs = enumerate_output_devices().expect("enumeration must not error");
        assert!(
            !devs.is_empty(),
            "HAL reports default output {default} but enumeration is empty \
             (size-query regression?)"
        );
        let entry = devs
            .iter()
            .find(|d| d.id == default.to_string())
            .unwrap_or_else(|| {
                panic!("default output {default} missing from enumeration: {devs:?}")
            });
        assert!(entry.is_default, "default entry not tagged: {entry:?}");
        assert!(
            !entry.name.is_empty(),
            "device name lookup failed for the default output (size-query \
             regression in hal_device_name?)"
        );
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
