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
- **Device enumeration beyond default.** `probe()` only checks whether
  the default device is usable.
- **Sample formats other than f32** on the public callback surface.
  Backends convert internally where the hardware insists on S16 or
  similar.
