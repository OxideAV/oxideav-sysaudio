# oxideav-sysaudio

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
| Linux   | OSS       | Stub        | `/dev/dsp` (not yet wired)                                                    |
| Windows | WASAPI    | Functional  | `ole32.dll` + `kernel32.dll` (COM vtables invoked by hand, shared-mode)       |
| Windows | ASIO      | Stub        | Vendor-supplied DLLs under `HKLM\SOFTWARE\ASIO` (not yet wired)               |
| macOS   | CoreAudio | Functional  | `AudioToolbox.framework` (AudioQueue API)                                     |

`probe()` returns the subset of these whose shared object loads AND
whose dummy-open succeeds, in the documented preference order:

- Linux: PipeWire → PulseAudio → ALSA → OSS
- Windows: WASAPI → ASIO
- macOS: CoreAudio

Stubbed backends fail `probe()` cleanly so auto-selection falls through
to the next working backend.

## Usage

```rust
use oxideav_sysaudio::{open_default, StreamRequest};

let req = StreamRequest::new(48_000, 2);
let mut stream = open_default(req, |out, _info| {
    out.fill(0.0); // silence
})?;
stream.play()?;
```

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

## Latency reporting

`Stream::latency()` returns an `Option<Duration>` describing how long a
submitted sample takes to reach the user's ears. Use it to compensate
A/V sync when the output sink has non-trivial delay (Bluetooth,
network PulseAudio, HDMI passthrough, etc.):

| Backend   | Source                                                                             | BT/Network-aware |
| --------- | ---------------------------------------------------------------------------------- | ---------------- |
| PulseAudio| `pa_simple_get_latency` (end-to-end, server-side)                                  | Yes              |
| ALSA      | `snd_pcm_delay` (driver queue depth)                                               | Partial          |
| WASAPI    | Live `IAudioClock::GetPosition` vs. frames-written delta (end-to-end, includes the device hardware pipeline). Falls back to `GetStreamLatency` + live `GetCurrentPadding` if the driver shim doesn't implement `IAudioClock`. | Yes              |
| CoreAudio | `num_buffers × period` + HAL (`kAudioDevicePropertyLatency` + buffer frame size + safety offset + stream latency) | Yes              |

## Features

All backends are enabled by default. Disabling a feature omits the
module entirely — useful for minimising binary size on platforms
where, e.g., you know you'll never need PulseAudio.

## Non-goals (for now)

- **Audio capture / input streams.** Output only.
- **Opening a non-default device.** Enumeration lists every output
  device (see above), but `open()` still binds the system default;
  routing to a specific enumerated `Device::id` is a follow-up.
- **PulseAudio device enumeration.** The "simple" API exposes no sink
  introspection; it returns an empty list. The full async
  `pa_context_get_sink_info_list` path is a follow-up.
- **Sample formats other than f32** on the public callback surface.
  Backends convert internally where the hardware insists on S16 or
  similar.
