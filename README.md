# oxideav-sysaudio

[![CI](https://github.com/OxideAV/oxideav-sysaudio/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-sysaudio/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-sysaudio.svg)](https://crates.io/crates/oxideav-sysaudio) [![docs.rs](https://docs.rs/oxideav-sysaudio/badge.svg)](https://docs.rs/oxideav-sysaudio) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Pure-Rust audio output for the `oxideav` workspace. Every native audio
API is loaded at runtime through `libloading`, so the produced binary
has **no** audio library listed in its ELF `NEEDED` entries (or the
Windows / macOS equivalents). No dev headers are required at build
time either — `cargo build` works on any platform regardless of which
audio SDK is installed.

## Backends

| Target  | Backend   | Status      | Shared object                                                                 |
| ------- | --------- | ----------- | ----------------------------------------------------------------------------- |
| Linux   | PipeWire  | Stub        | `libpipewire-0.3.so.0` (not yet wired)                                        |
| Linux   | PulseAudio| Functional  | `libpulse-simple.so.0`                                                        |
| Linux   | ALSA      | Functional  | `libasound.so.2`                                                              |
| Linux   | OSS       | Functional  | `/dev/dsp` via dlopen'd libc (`open`/`close`/`write`/`ioctl`)                 |
| Windows | WASAPI    | Functional  | `ole32.dll` + `kernel32.dll` (COM vtables invoked by hand, shared-mode)       |
| Windows | ASIO      | Stub        | Vendor-supplied DLLs under `HKLM\SOFTWARE\ASIO` (not yet wired)               |
| macOS   | CoreAudio | Functional  | `AudioToolbox.framework` (AudioQueue API)                                     |
| any     | Mock      | Test-only   | none — virtual discard/capture sink (non-default cargo feature `mock`)       |

`probe()` returns the subset of these whose shared object loads AND
whose dummy-open succeeds, in the documented preference order:

- Linux: PipeWire → PulseAudio → ALSA → OSS
- Windows: WASAPI → ASIO
- macOS: CoreAudio

Stubbed backends fail `probe()` cleanly so auto-selection falls through
to the next working backend. OSS is last in the Linux preference order
because on modern distros `/dev/dsp` is supplied by an OSS-emulator
sitting on top of ALSA — when ALSA itself is present, opening it
directly bypasses one level of indirection.

`Driver::is_stub()` reports whether a backend ships as a placeholder
(PipeWire, ASIO) versus a working implementation whose shared library
may or may not be installed on this host. The two cases look identical
through `probe()` alone — both fail — so callers iterating `drivers()`
to surface a per-backend UI label can use the compile-time flag to tell
"not yet implemented" apart from "library not installed".

`Driver::status()` composes all of that into one call — and, unlike
`probe()`, keeps hold of *why* a backend is unusable:

```rust
for d in oxideav_sysaudio::drivers() {
    // "ready" | "not yet implemented (stub)" | "unavailable: <probe error>"
    println!("{:10} {}", d.name(), d.status());
}
```

`DriverStatus::Unavailable(e)` carries the probe failure `probe()`
swallows: a `LibraryLoad` means the shared library isn't installed, a
`DeviceOpen` means the library loads but no device opens (headless
box, sound server down). Probing opens a throw-away handle, so call
`status()` for diagnostics/UI, not per audio period.

## Usage

```rust
use oxideav_sysaudio::{open_default, StreamRequest};

let req = StreamRequest::new(48_000, 2);
let mut stream = open_default(req, |out, _info| {
    out.fill(0.0); // silence
})?;
// Streams start in the playing state — the callback is already
// running. pause()/play() toggle it; is_playing() reports the last
// requested transport state.
stream.pause()?;
stream.play()?;
```

Requests no backend could satisfy (`sample_rate == 0`,
`channels == 0`, `buffer_frames == Some(0)`) are rejected up front
with `Error::UnsupportedFormat`, uniformly across backends, before any
shared library is loaded.

Explicit driver selection:

```rust
let d = oxideav_sysaudio::driver_by_name("alsa")
    .ok_or("alsa not compiled in")?;
let stream = oxideav_sysaudio::open(d, req, callback)?;
```

## Device enumeration

`Driver::output_devices()` (or the free function
`oxideav_sysaudio::output_devices(driver)`) lists the playback devices
the backend can see, each as a `Device { id, name, is_default }`:

```rust
let d = oxideav_sysaudio::default_driver().ok_or("no driver")?;
for dev in d.output_devices()? {
    let tag = if dev.is_default { " (default)" } else { "" };
    println!("{}{tag}  [{}]", dev.name, dev.id);
}
```

- `id` is the backend-native opaque token (an ALSA PCM name like
  `plughw:CARD=PCH,DEV=0`, a WASAPI endpoint id, a CoreAudio numeric
  `AudioDeviceID`). Treat it as opaque; do not parse it.
- `name` is the OS-friendly label shown in the sound settings.
- Exactly one entry in a non-empty list has `is_default == true` — the
  device a plain `open_default()` plays through.

| Target  | Backend   | Enumeration source                                                         |
| ------- | --------- | -------------------------------------------------------------------------- |
| Linux   | ALSA      | `snd_device_name_hint("pcm")`, filtered to `IOID=Output`/duplex            |
| Windows | WASAPI    | `IMMDeviceEnumerator::EnumAudioEndpoints(eRender, ACTIVE)` + `PKEY_Device_FriendlyName` |
| macOS   | CoreAudio | HAL `kAudioHardwarePropertyDevices`, kept where output streams exist       |

Backends that can only reach the default device (the PulseAudio
"simple" API) and the not-yet-wired stubs (PipeWire, OSS, ASIO) return
an **empty list** rather than an error, so a caller can union device
lists across every probed driver without per-backend special-casing.

`Driver::default_output_device()` is the one-call shortcut for the
common "where does the system play right now?" query — it returns the
entry from the enumeration whose `is_default` is set, without forcing
the caller to materialise the full list and filter:

```rust
let d = oxideav_sysaudio::default_driver().ok_or("no driver")?;
if let Some(dev) = d.default_output_device()? {
    println!("default sink: {} [{}]", dev.name, dev.id);
}
```

Backends without an enumeration path (PulseAudio simple API, stubs)
surface `Ok(None)` rather than an error, matching `output_devices()`
so an iterating caller can union per-driver results.

## Software volume

`Stream::set_volume(f32)` / `Stream::volume()` — a per-stream gain
stage between the callback and the backend. `1.0` (default) is unity
gain and bypasses the multiply entirely; `0.0` is silence; values
above `1.0` amplify and may clip; negative/NaN clamp to `0.0`. The
gain lives in an atomic the audio thread reads wait-free, so
`set_volume` is safe to call from a UI thread at any rate. It is
independent of the OS mixer volume and composes with it.

## Latency reporting

`Stream::latency()` returns an `Option<Duration>` describing how long a
submitted sample takes to reach the user's ears. Use it to compensate
A/V sync when the output sink has non-trivial delay (Bluetooth,
network PulseAudio, HDMI passthrough, etc.):

| Backend   | Source                                                                             | BT/Network-aware |
| --------- | ---------------------------------------------------------------------------------- | ---------------- |
| PulseAudio| `pa_simple_get_latency` (end-to-end, server-side)                                  | Yes              |
| ALSA      | `snd_pcm_delay` (driver queue depth)                                               | Partial          |
| OSS       | Worker-side period buffering (`period_frames / sample_rate`)                       | No               |
| WASAPI    | Live `IAudioClock::GetPosition` vs. frames-written delta (end-to-end, includes the device hardware pipeline). Falls back to `GetStreamLatency` + live `GetCurrentPadding` if the driver shim doesn't implement `IAudioClock`. | Yes              |
| CoreAudio | `num_buffers × period` + HAL (`kAudioDevicePropertyLatency` + buffer frame size + safety offset + stream latency) | Yes              |

## Features

All backends are enabled by default. Disabling a feature omits the
module entirely — useful for minimising binary size on platforms
where, e.g., you know you'll never need PulseAudio.

## Opening a specific device

`StreamRequest::with_device(id)` (or the lower-level `StreamRequest`
`device` field) binds the stream to a specific enumerated endpoint,
identified by the opaque `id` returned in `Device::id` by the same
backend's `output_devices()` call. The free `open_on(driver, &device,
req, cb)` is the natural shortcut when you already have a `Device`
in hand:

```rust
let d = oxideav_sysaudio::default_driver().ok_or("no driver")?;
for dev in d.output_devices()? {
    if dev.name.contains("USB Headset") {
        let req = oxideav_sysaudio::StreamRequest::new(48_000, 2);
        let stream = oxideav_sysaudio::open_on(d, &dev, req, |out, _| {
            out.fill(0.0);
        })?;
        // ... play through the USB headset specifically ...
        break;
    }
}
```

| Backend   | Per-device routing                                                              |
| --------- | ------------------------------------------------------------------------------- |
| ALSA      | `id` is the PCM name; passed straight to `snd_pcm_open`.                        |
| OSS       | `id` is the character-device path (e.g. `/dev/dsp1`, `/dev/dsp_hw0`); `None` opens `/dev/dsp`. |
| PulseAudio| `id` is a sink name; passed as the `dev` arg of `pa_simple_new`.                |
| WASAPI    | `id` is the LPWSTR endpoint id; resolved via `IMMDeviceEnumerator::GetDevice`.  |
| CoreAudio | `id` is the decimal `AudioDeviceID`; HAL `kAudioDevicePropertyDeviceUID` yields the CFString, then `AudioQueueSetProperty(kAudioQueueProperty_CurrentDevice, &cfstr)` binds the queue. `latency()` follows the bound device. |

Leaving `device` as `None` (the default constructor) opens the system
default endpoint, matching the historical `open()` / `open_default()`
behaviour.

## Buffer-size hint

`StreamRequest::with_buffer_frames(Some(n))` (or the underlying
`buffer_frames: Option<u32>` field) hands every functional backend a
period-size target measured in frames. The hint is advisory — each
backend translates it into its native unit and the OS / driver / mix
engine may clamp it to a supported value. Leaving it as `None`
preserves the historical backend defaults (roughly 20 ms across the
board).

| Backend   | Per-backend routing of `buffer_frames`                                                                                  |
| --------- | ----------------------------------------------------------------------------------------------------------------------- |
| ALSA      | `snd_pcm_hw_params_set_period_size_near` with the hint; buffer set to `4 × period`.                                     |
| OSS       | Worker write size = the hint; OSS has no separate period ioctl in the historic UAPI surface, so the per-`write` size IS the period. `None` keeps the historical ~20 ms target (`sample_rate / 50`, floored at 64 frames). |
| PulseAudio| Filled into a `pa_buffer_attr` (`tlength = frames × bytes_per_frame`, `minreq` ≈ one period and capped at `tlength`); other fields stay `(uint32_t)-1`. Worker write size follows the hint so client and server stay aligned. |
| WASAPI    | Translated to `REFERENCE_TIME` (100 ns ticks) via `frames × 10_000_000 / sample_rate` with i128 widening; passed as `hnsBufferDuration` to `IAudioClient::Initialize`. WASAPI clamps below the device's minimum period. |
| CoreAudio | `kAudioQueueProperty_NumberOfBuffers` × buffer size derived from the hint.                                              |

Sub-millisecond hints round up to at least one tick on WASAPI; massive
hints saturate rather than overflow.

## Sample-rate negotiation read-back

`Driver::preferred_format(Option<&Device>)` reports what `open()` would
agree on for an unconstrained request, without committing a stream.
Callers use it to resample once on their side and skip the OS mixer's
hidden conversion path:

```rust
let d = oxideav_sysaudio::default_driver().ok_or("no driver")?;
if let Some(fmt) = d.preferred_format(None)? {
    // Resample our 44.1 kHz pipeline to fmt.sample_rate before open().
    println!("native: {} Hz, {} ch", fmt.sample_rate, fmt.channels);
}
```

| Backend   | Source of the report                                                                                                       |
| --------- | -------------------------------------------------------------------------------------------------------------------------- |
| WASAPI    | `IAudioClient::GetMixFormat` (the shared-mode mix engine's preferred format — typically 48 kHz f32 stereo on Windows-10/11). |
| CoreAudio | HAL `kAudioDevicePropertyNominalSampleRate` for the rate + `kAudioStreamPropertyVirtualFormat` on the device's first output stream for the channel count. Aggregate devices stay coherent (per-stream rates may disagree). |
| ALSA      | Throwaway `snd_pcm_open` in `NONBLOCK` mode + `snd_pcm_hw_params_any` to load the device's full param space, then `snd_pcm_hw_params_set_rate_near(48000)` and `snd_pcm_hw_params_set_channels_near(2)` to read the snapped values out of their mutable args — the same path the real `open()` walks. The PCM is closed before the call returns. |
| Others    | `Ok(None)` — PulseAudio simple API exposes no sink introspection; OSS's `SNDCTL_DSP_*` family is in-band (committing the values to the device) so there's no read-without-write path; PipeWire/ASIO are stubs. |

A backend without an introspection path surfaces as `Ok(None)` rather
than an error so callers can iterate every probed driver without
per-backend special-casing.

## Testing without hardware — the `mock` backend

The non-default cargo feature `mock` compiles in a virtual `"mock"`
driver (any OS, no audio library, no device) that renders the callback
from a paced worker thread into a discard — or capture — sink. It
registers **last** in the preference order, so a real backend still
wins whenever one works; it is kept out of the default feature set on
purpose, because with it enabled `probe()` never comes back empty,
which would change `open_default()`'s failure behaviour for ordinary
users.

It enumerates three virtual devices (`mock:default`, `mock:secondary`,
`mock:capture`), honours `buffer_frames` hints verbatim, rejects
fabricated device ids, models `latency()` as a fixed two-period
software queue, and advances `CallbackInfo::frames_played` one period
per callback while playing. Streams opened on `mock:capture` copy
every rendered (post volume-gain) sample into a bounded global sink
drained via `oxideav_sysaudio::mock::take_captured()` — this is how
the crate's own tests verify the render pipeline sample-exactly.

`tests/mock_backend.rs` drives the full public state machine through
it on hardware-free CI; `tests/hardware_smoke.rs` runs the same
lifecycle against the first *real* backend that probes and skips
cleanly on headless runners (set `OXIDEAV_SYSAUDIO_STRICT_HW=1`
locally to make environment-dependent observations hard failures).

## Non-goals (for now)

- **Audio capture / input streams.** Output only.
- **PulseAudio device enumeration.** The "simple" API exposes no sink
  introspection; it returns an empty list. The full async
  `pa_context_get_sink_info_list` path is a follow-up.
- **Sample formats other than f32** on the public callback surface.
  Backends convert internally where the hardware insists on S16 or
  similar.

## License

MIT. See [LICENSE](LICENSE).
