//! WASAPI output backend.
//!
//! Uses `ole32.dll` (`CoInitializeEx`, `CoCreateInstance`,
//! `CoTaskMemFree`) and `kernel32.dll` (`CreateEventA`, `SetEvent`,
//! `WaitForSingleObject`, `CloseHandle`) through libloading — no COM
//! bindings crate, no winapi, no windows-rs. The COM interfaces
//! (`IMMDeviceEnumerator`, `IMMDevice`, `IAudioClient`,
//! `IAudioRenderClient`) are declared as bare `#[repr(C)]` vtables and
//! invoked by hand.
//!
//! Shared mode, event-driven. The device's mix format (almost always
//! f32 stereo at the user's system sample rate) is accepted as-is; if
//! the device insists on S16 we convert, anything else errors out
//! rather than guessing.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::upper_case_acronyms)]

use std::ffi::{c_char, c_int, c_long, c_longlong, c_ulong, c_void, CString};
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
// Win32 primitives
// ---------------------------------------------------------------------------

type HRESULT = c_long;
type DWORD = c_ulong;
type BOOL = c_int;
type HANDLE = *mut c_void;
type REFERENCE_TIME = c_longlong;

const S_OK: HRESULT = 0;
const WAIT_OBJECT_0: DWORD = 0;
const INFINITE: DWORD = 0xFFFF_FFFF;

const COINIT_MULTITHREADED: DWORD = 0x0;
const CLSCTX_ALL: DWORD = 0x17;
const AUDCLNT_SHAREMODE_SHARED: c_int = 0;
const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: DWORD = 0x0004_0000;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct GUID {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// Helper — nothing in this module introspects GUIDs beyond the values
/// required by the COM calls, so we hard-code them instead of parsing
/// strings.
const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID {
        data1: d1,
        data2: d2,
        data3: d3,
        data4: d4,
    }
}

const CLSID_MMDeviceEnumerator: GUID = guid(
    0xBCDE_0395,
    0xE52F,
    0x467C,
    [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
);
const IID_IMMDeviceEnumerator: GUID = guid(
    0xA956_64D2,
    0x9614,
    0x4F35,
    [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
);
const IID_IAudioClient: GUID = guid(
    0x1CB9_AD4C,
    0xDBFA,
    0x4C32,
    [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
);
const IID_IAudioRenderClient: GUID = guid(
    0xF294_ACFC,
    0x3146,
    0x4483,
    [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2],
);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = guid(
    0x0000_0003,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);
const KSDATAFORMAT_SUBTYPE_PCM: GUID = guid(
    0x0000_0001,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct WAVEFORMATEX {
    wFormatTag: u16,
    nChannels: u16,
    nSamplesPerSec: u32,
    nAvgBytesPerSec: u32,
    nBlockAlign: u16,
    wBitsPerSample: u16,
    cbSize: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct WAVEFORMATEXTENSIBLE {
    Format: WAVEFORMATEX,
    Samples: u16, // wValidBitsPerSample (union, same size)
    dwChannelMask: u32,
    SubFormat: GUID,
}

// ---------------------------------------------------------------------------
// COM vtables
// ---------------------------------------------------------------------------

// Every vtable starts with IUnknown's three methods in this order.
#[repr(C)]
struct IUnknownVtbl {
    QueryInterface: unsafe extern "system" fn(
        this: *mut c_void,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT,
    AddRef: unsafe extern "system" fn(this: *mut c_void) -> c_ulong,
    Release: unsafe extern "system" fn(this: *mut c_void) -> c_ulong,
}

#[repr(C)]
struct IMMDeviceEnumeratorVtbl {
    parent: IUnknownVtbl,
    EnumAudioEndpoints: unsafe extern "system" fn(
        this: *mut c_void,
        dataFlow: c_int,
        dwStateMask: DWORD,
        ppDevices: *mut *mut c_void,
    ) -> HRESULT,
    GetDefaultAudioEndpoint: unsafe extern "system" fn(
        this: *mut c_void,
        dataFlow: c_int,
        role: c_int,
        ppEndpoint: *mut *mut c_void,
    ) -> HRESULT,
    GetDevice: unsafe extern "system" fn(
        this: *mut c_void,
        pwstrId: *const u16,
        ppDevice: *mut *mut c_void,
    ) -> HRESULT,
    RegisterEndpointNotificationCallback:
        unsafe extern "system" fn(this: *mut c_void, pClient: *mut c_void) -> HRESULT,
    UnregisterEndpointNotificationCallback:
        unsafe extern "system" fn(this: *mut c_void, pClient: *mut c_void) -> HRESULT,
}

#[repr(C)]
struct IMMDeviceVtbl {
    parent: IUnknownVtbl,
    Activate: unsafe extern "system" fn(
        this: *mut c_void,
        iid: *const GUID,
        dwClsCtx: DWORD,
        pActivationParams: *mut c_void,
        ppInterface: *mut *mut c_void,
    ) -> HRESULT,
    OpenPropertyStore: unsafe extern "system" fn(
        this: *mut c_void,
        stgmAccess: DWORD,
        ppProperties: *mut *mut c_void,
    ) -> HRESULT,
    GetId: unsafe extern "system" fn(this: *mut c_void, ppstrId: *mut *mut u16) -> HRESULT,
    GetState: unsafe extern "system" fn(this: *mut c_void, pdwState: *mut DWORD) -> HRESULT,
}

#[repr(C)]
struct IAudioClientVtbl {
    parent: IUnknownVtbl,
    Initialize: unsafe extern "system" fn(
        this: *mut c_void,
        ShareMode: c_int,
        StreamFlags: DWORD,
        hnsBufferDuration: REFERENCE_TIME,
        hnsPeriodicity: REFERENCE_TIME,
        pFormat: *const WAVEFORMATEX,
        AudioSessionGuid: *const GUID,
    ) -> HRESULT,
    GetBufferSize:
        unsafe extern "system" fn(this: *mut c_void, pNumBufferFrames: *mut u32) -> HRESULT,
    GetStreamLatency:
        unsafe extern "system" fn(this: *mut c_void, phnsLatency: *mut REFERENCE_TIME) -> HRESULT,
    GetCurrentPadding:
        unsafe extern "system" fn(this: *mut c_void, pNumPaddingFrames: *mut u32) -> HRESULT,
    IsFormatSupported: unsafe extern "system" fn(
        this: *mut c_void,
        ShareMode: c_int,
        pFormat: *const WAVEFORMATEX,
        ppClosestMatch: *mut *mut WAVEFORMATEX,
    ) -> HRESULT,
    GetMixFormat: unsafe extern "system" fn(
        this: *mut c_void,
        ppDeviceFormat: *mut *mut WAVEFORMATEX,
    ) -> HRESULT,
    GetDevicePeriod: unsafe extern "system" fn(
        this: *mut c_void,
        phnsDefaultDevicePeriod: *mut REFERENCE_TIME,
        phnsMinimumDevicePeriod: *mut REFERENCE_TIME,
    ) -> HRESULT,
    Start: unsafe extern "system" fn(this: *mut c_void) -> HRESULT,
    Stop: unsafe extern "system" fn(this: *mut c_void) -> HRESULT,
    Reset: unsafe extern "system" fn(this: *mut c_void) -> HRESULT,
    SetEventHandle: unsafe extern "system" fn(this: *mut c_void, eventHandle: HANDLE) -> HRESULT,
    GetService: unsafe extern "system" fn(
        this: *mut c_void,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT,
}

#[repr(C)]
struct IAudioRenderClientVtbl {
    parent: IUnknownVtbl,
    GetBuffer: unsafe extern "system" fn(
        this: *mut c_void,
        NumFramesRequested: u32,
        ppData: *mut *mut u8,
    ) -> HRESULT,
    ReleaseBuffer: unsafe extern "system" fn(
        this: *mut c_void,
        NumFramesWritten: u32,
        dwFlags: DWORD,
    ) -> HRESULT,
}

// COM objects are pointers to pointers to vtables.
#[repr(C)]
struct ComObj<V> {
    vtbl: *const V,
}

// ---------------------------------------------------------------------------
// Loaded libraries — ole32 + kernel32
// ---------------------------------------------------------------------------

type Fn_CoInitializeEx =
    unsafe extern "system" fn(pvReserved: *mut c_void, dwCoInit: DWORD) -> HRESULT;
type Fn_CoUninitialize = unsafe extern "system" fn();
type Fn_CoCreateInstance = unsafe extern "system" fn(
    rclsid: *const GUID,
    pUnkOuter: *mut c_void,
    dwClsContext: DWORD,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT;
type Fn_CoTaskMemFree = unsafe extern "system" fn(pv: *mut c_void);

type Fn_CreateEventA = unsafe extern "system" fn(
    lpEventAttributes: *mut c_void,
    bManualReset: BOOL,
    bInitialState: BOOL,
    lpName: *const c_char,
) -> HANDLE;
type Fn_CloseHandle = unsafe extern "system" fn(hObject: HANDLE) -> BOOL;
type Fn_WaitForSingleObject =
    unsafe extern "system" fn(hHandle: HANDLE, dwMilliseconds: DWORD) -> DWORD;
type Fn_SetEvent = unsafe extern "system" fn(hEvent: HANDLE) -> BOOL;

struct WinLibs {
    _ole32: Library,
    _kernel32: Library,
    CoInitializeEx: Fn_CoInitializeEx,
    CoUninitialize: Fn_CoUninitialize,
    CoCreateInstance: Fn_CoCreateInstance,
    CoTaskMemFree: Fn_CoTaskMemFree,
    CreateEventA: Fn_CreateEventA,
    CloseHandle: Fn_CloseHandle,
    WaitForSingleObject: Fn_WaitForSingleObject,
    SetEvent: Fn_SetEvent,
}

unsafe impl Send for WinLibs {}
unsafe impl Sync for WinLibs {}

impl WinLibs {
    fn load() -> Result<Arc<Self>> {
        unsafe {
            let ole32 = Library::new("ole32.dll").map_err(|e| Error::LibraryLoad {
                backend: "wasapi",
                soname: "ole32.dll",
                source: e,
            })?;
            let kernel32 = Library::new("kernel32.dll").map_err(|e| Error::LibraryLoad {
                backend: "wasapi",
                soname: "kernel32.dll",
                source: e,
            })?;
            macro_rules! sym {
                ($lib:ident, $name:literal, $ty:ty) => {{
                    let s: Symbol<$ty> =
                        $lib.get(concat!($name, "\0").as_bytes()).map_err(|e| {
                            Error::SymbolMissing {
                                backend: "wasapi",
                                symbol: $name,
                                source: e,
                            }
                        })?;
                    *s
                }};
            }
            Ok(Arc::new(WinLibs {
                CoInitializeEx: sym!(ole32, "CoInitializeEx", Fn_CoInitializeEx),
                CoUninitialize: sym!(ole32, "CoUninitialize", Fn_CoUninitialize),
                CoCreateInstance: sym!(ole32, "CoCreateInstance", Fn_CoCreateInstance),
                CoTaskMemFree: sym!(ole32, "CoTaskMemFree", Fn_CoTaskMemFree),
                CreateEventA: sym!(kernel32, "CreateEventA", Fn_CreateEventA),
                CloseHandle: sym!(kernel32, "CloseHandle", Fn_CloseHandle),
                WaitForSingleObject: sym!(kernel32, "WaitForSingleObject", Fn_WaitForSingleObject),
                SetEvent: sym!(kernel32, "SetEvent", Fn_SetEvent),
                _ole32: ole32,
                _kernel32: kernel32,
            }))
        }
    }
}

fn lib() -> Result<Arc<WinLibs>> {
    static CACHED: OnceLock<Mutex<Option<Arc<WinLibs>>>> = OnceLock::new();
    let slot = CACHED.get_or_init(|| Mutex::new(None));
    let mut g = slot.lock().unwrap();
    if let Some(l) = g.as_ref() {
        return Ok(l.clone());
    }
    let l = WinLibs::load()?;
    *g = Some(l.clone());
    Ok(l)
}

// Convenience wrappers around the vtable indirections.
unsafe fn com_release(obj: *mut c_void) {
    if obj.is_null() {
        return;
    }
    let vtbl = *(obj as *mut *const IUnknownVtbl);
    ((*vtbl).Release)(obj);
}

// Helper: call CoInitializeEx once per thread. Return value can be
// S_OK, S_FALSE (already initialized — still fine), or RPC_E_CHANGED_MODE
// which we tolerate.
unsafe fn co_init(l: &WinLibs) -> HRESULT {
    (l.CoInitializeEx)(ptr::null_mut(), COINIT_MULTITHREADED)
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub(crate) struct WasapiBackend;

impl Backend for WasapiBackend {
    fn name(&self) -> &'static str {
        "wasapi"
    }
    fn description(&self) -> &'static str {
        "WASAPI (ole32.dll + mmdevapi via libloading, shared mode, event-driven)"
    }

    fn probe(&self) -> Result<()> {
        let l = lib()?;
        unsafe {
            co_init(&l);
            let (client, _wfx) = activate_default_client(&l)?;
            com_release(client);
            (l.CoUninitialize)();
        }
        Ok(())
    }

    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>> {
        let l = lib()?;
        unsafe { open_inner(l, req, cb) }
    }
}

/// Resolve IMMDeviceEnumerator → default endpoint → IAudioClient and
/// its mix format. Caller owns both the returned client (must be
/// Released) and the WAVEFORMATEX* (must be CoTaskMemFree'd — we return
/// it boxed so the caller can reuse it for Initialize).
unsafe fn activate_default_client(l: &WinLibs) -> Result<(*mut c_void, *mut WAVEFORMATEX)> {
    // 1. CoCreateInstance(CLSID_MMDeviceEnumerator, …, IID_IMMDeviceEnumerator)
    let mut enumer: *mut c_void = ptr::null_mut();
    let hr = (l.CoCreateInstance)(
        &CLSID_MMDeviceEnumerator,
        ptr::null_mut(),
        CLSCTX_ALL,
        &IID_IMMDeviceEnumerator,
        &mut enumer,
    );
    if hr != S_OK || enumer.is_null() {
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: format!("CoCreateInstance(MMDeviceEnumerator) HRESULT=0x{hr:08X}"),
        });
    }

    // 2. GetDefaultAudioEndpoint(eRender=0, eConsole=0, &pDevice)
    let enumer_vtbl = *(enumer as *mut *const IMMDeviceEnumeratorVtbl);
    let mut device: *mut c_void = ptr::null_mut();
    let hr = ((*enumer_vtbl).GetDefaultAudioEndpoint)(enumer, 0, 0, &mut device);
    if hr != S_OK || device.is_null() {
        com_release(enumer);
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: format!("GetDefaultAudioEndpoint HRESULT=0x{hr:08X}"),
        });
    }
    com_release(enumer);

    // 3. IMMDevice::Activate(IID_IAudioClient, CLSCTX_ALL, null, &pClient)
    let device_vtbl = *(device as *mut *const IMMDeviceVtbl);
    let mut client: *mut c_void = ptr::null_mut();
    let hr = ((*device_vtbl).Activate)(
        device,
        &IID_IAudioClient,
        CLSCTX_ALL,
        ptr::null_mut(),
        &mut client,
    );
    com_release(device);
    if hr != S_OK || client.is_null() {
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: format!("IMMDevice::Activate HRESULT=0x{hr:08X}"),
        });
    }

    // 4. IAudioClient::GetMixFormat(&pwfx)
    let client_vtbl = *(client as *mut *const IAudioClientVtbl);
    let mut pwfx: *mut WAVEFORMATEX = ptr::null_mut();
    let hr = ((*client_vtbl).GetMixFormat)(client, &mut pwfx);
    if hr != S_OK || pwfx.is_null() {
        com_release(client);
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: format!("GetMixFormat HRESULT=0x{hr:08X}"),
        });
    }
    Ok((client, pwfx))
}

/// Read the effective sample format out of a WAVEFORMATEX (handling
/// WAVEFORMATEXTENSIBLE's SubFormat GUID). Returns (is_float, bits).
unsafe fn inspect_format(pwfx: *const WAVEFORMATEX) -> Option<(bool, u16)> {
    let tag = (*pwfx).wFormatTag;
    let bits = (*pwfx).wBitsPerSample;
    match tag {
        WAVE_FORMAT_IEEE_FLOAT => Some((true, bits)),
        WAVE_FORMAT_PCM => Some((false, bits)),
        WAVE_FORMAT_EXTENSIBLE => {
            let ext = pwfx as *const WAVEFORMATEXTENSIBLE;
            let sub = (*ext).SubFormat;
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                Some((true, bits))
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                Some((false, bits))
            } else {
                None
            }
        }
        _ => None,
    }
}

unsafe fn open_inner(
    l: Arc<WinLibs>,
    _req: StreamRequest,
    cb: Callback,
) -> Result<Box<dyn StreamImpl>> {
    co_init(&l);
    let (client, pwfx) = activate_default_client(&l)?;
    let channels = (*pwfx).nChannels;
    let sample_rate = (*pwfx).nSamplesPerSec;
    let (is_float, bits) = match inspect_format(pwfx) {
        Some(v) => v,
        None => {
            (l.CoTaskMemFree)(pwfx as *mut c_void);
            com_release(client);
            return Err(Error::UnsupportedFormat {
                backend: "wasapi",
                detail: "mix format is neither IEEE_FLOAT nor PCM".into(),
            });
        }
    };
    // We only handle f32 and s16 in the worker. Anything else → bail
    // rather than produce mangled audio.
    let is_f32 = is_float && bits == 32;
    let is_s16 = !is_float && bits == 16;
    if !is_f32 && !is_s16 {
        (l.CoTaskMemFree)(pwfx as *mut c_void);
        com_release(client);
        return Err(Error::UnsupportedFormat {
            backend: "wasapi",
            detail: format!("mix format bits_per_sample={bits} is_float={is_float}"),
        });
    }

    // Ask for a 200 ms buffer — WASAPI will clamp to the device's
    // minimum period internally. REFERENCE_TIME is 100ns units.
    let buffer_duration: REFERENCE_TIME = 200 * 10_000;

    let client_vtbl = *(client as *mut *const IAudioClientVtbl);
    let hr = ((*client_vtbl).Initialize)(
        client,
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        buffer_duration,
        0,
        pwfx,
        ptr::null(),
    );
    (l.CoTaskMemFree)(pwfx as *mut c_void);
    if hr != S_OK {
        com_release(client);
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: format!("IAudioClient::Initialize HRESULT=0x{hr:08X}"),
        });
    }

    // Event handle — auto-reset so we block exactly once per buffer fill.
    let event = (l.CreateEventA)(ptr::null_mut(), 0, 0, ptr::null());
    if event.is_null() {
        com_release(client);
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: "CreateEventA returned NULL".into(),
        });
    }
    let hr = ((*client_vtbl).SetEventHandle)(client, event);
    if hr != S_OK {
        (l.CloseHandle)(event);
        com_release(client);
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: format!("SetEventHandle HRESULT=0x{hr:08X}"),
        });
    }

    let mut buffer_frames: u32 = 0;
    let hr = ((*client_vtbl).GetBufferSize)(client, &mut buffer_frames);
    if hr != S_OK {
        (l.CloseHandle)(event);
        com_release(client);
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: format!("GetBufferSize HRESULT=0x{hr:08X}"),
        });
    }

    let mut render: *mut c_void = ptr::null_mut();
    let hr = ((*client_vtbl).GetService)(client, &IID_IAudioRenderClient, &mut render);
    if hr != S_OK || render.is_null() {
        (l.CloseHandle)(event);
        com_release(client);
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: format!("GetService(IAudioRenderClient) HRESULT=0x{hr:08X}"),
        });
    }

    let hr = ((*client_vtbl).Start)(client);
    if hr != S_OK {
        com_release(render);
        (l.CloseHandle)(event);
        com_release(client);
        return Err(Error::DeviceOpen {
            backend: "wasapi",
            detail: format!("IAudioClient::Start HRESULT=0x{hr:08X}"),
        });
    }

    // GetStreamLatency is stable after Initialize — reports the
    // driver/hardware-side delay in REFERENCE_TIME (100 ns units).
    // Over Bluetooth this includes the A2DP round-trip.
    let mut stream_latency_refs: REFERENCE_TIME = 0;
    let _ = ((*client_vtbl).GetStreamLatency)(client, &mut stream_latency_refs);
    let stream_latency_ns = (stream_latency_refs.max(0) as u64) * 100;

    let stop = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let frames_played = Arc::new(AtomicU64::new(0));
    // Latest GetCurrentPadding in frames — read by the worker and
    // published for `latency()`.
    let padding_frames = Arc::new(AtomicU64::new(0));

    let state = WasapiWorkerState {
        lib: l.clone(),
        client: ComPtr(client),
        render: ComPtr(render),
        event: HandlePtr(event),
        buffer_frames,
        channels: channels as usize,
        is_f32,
        stop: stop.clone(),
        paused: paused.clone(),
        frames_played: frames_played.clone(),
        padding_frames: padding_frames.clone(),
        cb,
    };

    let thread = std::thread::Builder::new()
        .name("oxideav-sysaudio-wasapi".into())
        .spawn(move || state.run())
        .map_err(|e| Error::Runtime {
            backend: "wasapi",
            detail: format!("spawn worker: {e}"),
        })?;

    Ok(Box::new(WasapiStream {
        lib: l,
        client: ComPtr(client),
        render: ComPtr(render),
        event: HandlePtr(event),
        stop,
        paused,
        thread: Some(thread),
        padding_frames,
        stream_latency_ns,
        format: StreamFormat {
            sample_rate,
            channels,
            format: SampleFormat::F32,
        },
    }))
}

#[derive(Copy, Clone)]
struct ComPtr(*mut c_void);
unsafe impl Send for ComPtr {}

#[derive(Copy, Clone)]
struct HandlePtr(HANDLE);
unsafe impl Send for HandlePtr {}

struct WasapiWorkerState {
    lib: Arc<WinLibs>,
    client: ComPtr,
    render: ComPtr,
    event: HandlePtr,
    buffer_frames: u32,
    channels: usize,
    is_f32: bool,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    frames_played: Arc<AtomicU64>,
    padding_frames: Arc<AtomicU64>,
    cb: Callback,
}

impl WasapiWorkerState {
    fn run(mut self) {
        unsafe {
            // Reuse one f32 scratch across iterations; the max we ever
            // need is the whole buffer worth.
            let mut f32_scratch = vec![0.0f32; self.buffer_frames as usize * self.channels];
            let client_vtbl = *(self.client.0 as *mut *const IAudioClientVtbl);
            let render_vtbl = *(self.render.0 as *mut *const IAudioRenderClientVtbl);

            while !self.stop.load(Ordering::Relaxed) {
                // 100 ms timeout so we notice `stop` promptly even if
                // the server stops pulling.
                let w = (self.lib.WaitForSingleObject)(self.event.0, 100);
                if w != WAIT_OBJECT_0 && w != 258
                /* WAIT_TIMEOUT */
                {
                    return;
                }
                let mut padding: u32 = 0;
                let hr = ((*client_vtbl).GetCurrentPadding)(self.client.0, &mut padding);
                if hr != S_OK {
                    return;
                }
                self.padding_frames.store(padding as u64, Ordering::Relaxed);
                let avail = self.buffer_frames.saturating_sub(padding);
                if avail == 0 {
                    continue;
                }

                let mut data: *mut u8 = ptr::null_mut();
                let hr = ((*render_vtbl).GetBuffer)(self.render.0, avail, &mut data);
                if hr != S_OK || data.is_null() {
                    return;
                }

                let samples = avail as usize * self.channels;
                if f32_scratch.len() < samples {
                    f32_scratch.resize(samples, 0.0);
                }
                let slice = &mut f32_scratch[..samples];

                if self.paused.load(Ordering::Relaxed) {
                    for s in slice.iter_mut() {
                        *s = 0.0;
                    }
                } else {
                    let info = CallbackInfo {
                        frames_played: self.frames_played.load(Ordering::Relaxed),
                    };
                    (self.cb)(slice, &info);
                }

                if self.is_f32 {
                    ptr::copy_nonoverlapping(slice.as_ptr() as *const u8, data, samples * 4);
                } else {
                    // S16LE fallback.
                    let dst = std::slice::from_raw_parts_mut(data as *mut i16, samples);
                    for (d, &s) in dst.iter_mut().zip(slice.iter()) {
                        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i32;
                        *d = v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    }
                }

                let hr = ((*render_vtbl).ReleaseBuffer)(self.render.0, avail, 0);
                if hr != S_OK {
                    return;
                }
                if !self.paused.load(Ordering::Relaxed) {
                    self.frames_played
                        .fetch_add(avail as u64, Ordering::Relaxed);
                }
            }
        }
    }
}

struct WasapiStream {
    lib: Arc<WinLibs>,
    client: ComPtr,
    render: ComPtr,
    event: HandlePtr,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    padding_frames: Arc<AtomicU64>,
    /// Cached IAudioClient::GetStreamLatency result, in nanoseconds.
    /// Stable for the lifetime of the stream.
    stream_latency_ns: u64,
    format: StreamFormat,
}

unsafe impl Send for WasapiStream {}

impl StreamImpl for WasapiStream {
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
        // Total output latency ≈ GetStreamLatency (fixed driver/device
        // delay, includes the Bluetooth A2DP pipeline on BT sinks) +
        // current buffer padding in nanoseconds.
        let rate = self.format.sample_rate.max(1) as u64;
        let padding = self.padding_frames.load(Ordering::Relaxed);
        let padding_ns = padding.saturating_mul(1_000_000_000) / rate;
        Some(Duration::from_nanos(self.stream_latency_ns + padding_ns))
    }
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        unsafe {
            let _ = (self.lib.SetEvent)(self.event.0);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            let client_vtbl = *(self.client.0 as *mut *const IAudioClientVtbl);
            let _ = ((*client_vtbl).Stop)(self.client.0);
            com_release(self.render.0);
            com_release(self.client.0);
            let _ = (self.lib.CloseHandle)(self.event.0);
            // Balance the CoInitializeEx we did in open_inner for this
            // thread. The worker thread didn't co_init, so there's no
            // matching Uninit there — only here.
            (self.lib.CoUninitialize)();
        }
    }
}
