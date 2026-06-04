# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- *(api)* `Driver::default_output_device() -> Result<Option<Device>>` —
  one-call shortcut for "the entry from `output_devices()` whose
  `is_default` flag is set", so a caller only interested in the current
  default endpoint doesn't have to materialise the full device list and
  filter. The default implementation on the internal `Backend` trait is
  exactly that reduction (every enumerating backend already tags the
  default entry), with room for backends to swap in a cheaper direct
  query later (CoreAudio's `kAudioHardwarePropertyDefaultOutputDevice`
  system-object selector, WASAPI's
  `IMMDeviceEnumerator::GetDefaultAudioEndpoint`). Public-layer mapping
  matches the rest of the surface: backends without an enumeration path
  (PulseAudio "simple" API, the not-yet-wired PipeWire/OSS/ASIO stubs)
  surface `Ok(None)` rather than an error so a caller can union
  per-driver results without per-backend special-casing. Three new unit
  tests pin the contract: `NotImplemented` is never leaked to the
  public surface, the shortcut agrees with the `is_default` entry from
  the enumeration (cross-check), and a `Some(_)` return always carries
  `is_default == true` (so a UI caller plumbing the returned `Device`
  into a label never mislabels it).
- *(oss)* OSS backend goes functional. `/dev/dsp` is opened directly via
  the Linux kernel UAPI (`<sys/soundcard.h>`); userspace surface is
  `open`/`close`/`write`/`ioctl` reached through `libloading` against
  `libc.so.6` / `libc.so` / `libc.musl-x86_64.so.1` so the produced
  binary still has no audio library in its NEEDED list. Format
  negotiation goes through `SNDCTL_DSP_SETFMT` (AFMT_S16_LE — the one
  format every OSS-emulator on top of ALSA advertises),
  `SNDCTL_DSP_CHANNELS`, and `SNDCTL_DSP_SPEED`; the worker thread
  converts the public f32 callback samples to S16_LE before each
  `write(2)`. `Stream::pause` skips the user callback and writes silence
  (OSS has no soft-cork); `Stream::stop` issues `SNDCTL_DSP_RESET` to
  drop the tail buffer rather than blocking on drain. `Stream::latency`
  reports the worker-side period buffering
  (`period_frames / sample_rate`); hardware-side delay via
  `SNDCTL_DSP_GETODELAY` is a follow-up. Per-device routing falls out
  of OSS's naming convention: `StreamRequest::with_device("/dev/dsp1")`
  binds the stream to an alternate character device. The ioctl request
  numbers are computed at const time via a local `_IOC(dir, type, nr,
  size)` packing function so the values are derived from the kernel
  ABI macro rather than transcribed hex; unit tests then assert the
  derivation against the documented `_IOWR('P', N, int)` results
  (0xC0045002 / 0xC0045005 / 0xC0045006 for SPEED / SETFMT /
  CHANNELS). OSS stays last in the Linux probe order — every modern
  distro's `/dev/dsp` is an emulator on top of ALSA, so opening ALSA
  directly bypasses one level of indirection when both are present.
  Closes the long-standing OSS-stub non-goal.
- *(alsa)* `preferred_format` now wired. A throwaway `snd_pcm_open` in
  `NONBLOCK` mode + `snd_pcm_hw_params_any` loads the device's full
  hw_params space, then `snd_pcm_hw_params_set_rate_near(48000)` and
  the newly-resolved `snd_pcm_hw_params_set_channels_near(2)` write the
  device's snapped rate / channel count back through their mutable args
  — exactly the same path the real `open()` walks, just without
  committing the params or starting the worker. The PCM is closed
  before the call returns so a follow-up real `open()` against the same
  device doesn't contend with the probe. Old libasound that lacks
  `set_channels_near` degrades to "channels = 2" rather than erroring
  the load (consistent with the existing degradation pattern for the
  hint API). Closes the previous ALSA gap in the sample-rate
  negotiation read-back table.
- *(api)* sample-rate negotiation read-back via
  `Driver::preferred_format(Option<&Device>) -> Result<Option<StreamFormat>>`.
  Returns the rate / channels / format the backend would settle on for an
  unconstrained `open()` against the system default (`None`) or the
  enumerated endpoint — so callers can resample on their side once and
  skip the OS mixer's hidden conversion. WASAPI reads
  `IAudioClient::GetMixFormat` (= the shared-mode mix engine's
  preferred format, typically 48 kHz f32 stereo on Windows-10/11);
  CoreAudio reads the HAL property
  `kAudioDevicePropertyNominalSampleRate` (`'nsrt'`) for the rate and
  `kAudioStreamPropertyVirtualFormat` (`'sfmt'`) on the device's first
  output stream for the channel count, so aggregate devices stay
  coherent. Backends without an introspection path (PulseAudio simple
  API, ALSA, the PipeWire/OSS/ASIO stubs) report `Ok(None)` so callers
  can iterate every driver without per-backend special-casing.
- *(wasapi, pulse)* honour `StreamRequest::buffer_frames` on every
  functional backend. WASAPI translates the hint into the
  `REFERENCE_TIME` (100 ns ticks) value `IAudioClient::Initialize`
  consumes via `frames × 10_000_000 / sample_rate` with `i128` widening
  (so a 30-min hint at 192 kHz still fits without overflow and a
  sub-millisecond hint rounds up to at least one tick); `None` keeps the
  historical ~200 ms target. PulseAudio fills a `pa_buffer_attr` with
  `tlength` set to the requested byte count and `minreq` set to roughly
  one period (capped at `tlength` so the server doesn't reject
  `minreq > tlength`); the worker's write size follows the hint so
  client and server stay aligned. ALSA and CoreAudio already wired the
  hint in prior rounds — every functional backend now covers it.
- *(coreaudio)* per-device routing now wired. The numeric `AudioDeviceID`
  exposed in `Device::id` is resolved through the HAL property
  `kAudioDevicePropertyDeviceUID` to a CFStringRef, then handed to
  `AudioQueueSetProperty(queue, kAudioQueueProperty_CurrentDevice, &cfstr)`
  before `AudioQueueStart`. CoreFoundation is `dlopen`'d at runtime
  through a new minimal `CfLib` (only `CFRelease`) so the no-link-time-deps
  invariant still holds — no `objc`, no `core-foundation` crate, the
  produced binary lists nothing CF-related in its load commands. The HAL
  latency query (`latency()`) now follows the bound device when one was
  set, so the figure stays correct on USB DACs / Bluetooth headphones
  routed via `open_on`. Non-numeric `Device::id` strings (callers
  fabricating ids, or routing strings from another backend) surface as
  `Error::UnsupportedFormat` rather than silently falling back to the
  system default. Closes the previous "CoreAudio per-device routing"
  non-goal.
- *(api)* per-device opening: `StreamRequest::with_device(id)` (and the
  underlying `StreamRequest::device: Option<String>`) plus the free
  `open_on(driver, &device, req, cb)` convenience. Closes the previous
  "non-default device" non-goal — callers can pipe an enumerated
  `Device::id` straight back into `open()`. ALSA routes the id as the
  PCM name to `snd_pcm_open`; PulseAudio passes it as the `dev` arg of
  `pa_simple_new`; WASAPI resolves it through
  `IMMDeviceEnumerator::GetDevice` (LPWSTR); CoreAudio resolves the
  numeric `AudioDeviceID` to a CFString UID via the HAL and binds the
  AudioQueue with `kAudioQueueProperty_CurrentDevice` (see the
  *(coreaudio)* entry above). `StreamRequest` is now `Clone` (no longer
  `Copy`) so it can carry an owned `String` id.
- *(api)* output-device enumeration: `Device { id, name, is_default }`,
  `Driver::output_devices()` and the free `output_devices(driver)`. Each
  backend lists its playback endpoints with the OS-friendly name and a
  flag for the system default; backends that can only reach the default
  (PulseAudio "simple", the PipeWire/OSS/ASIO stubs) return an empty list
  rather than an error so callers can union across drivers.
- *(alsa)* enumerate via `snd_device_name_hint("pcm")`, filtered to
  `IOID=Output`/duplex; PCM `NAME` is the `id`, the first `DESC` line the
  friendly name, `"default"` tagged as the system default. The malloc'd
  hint strings are released through a runtime-resolved libc `free` (no
  `libc` crate — same no-link-time-deps premise as the rest).
- *(wasapi)* enumerate active render endpoints via
  `IMMDeviceEnumerator::EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)`,
  reading `PKEY_Device_FriendlyName` from each `IPropertyStore` and
  tagging the `GetDefaultAudioEndpoint` match. New hand-rolled
  `IMMDeviceCollection` / `IPropertyStore` vtables + a minimal
  `PROPVARIANT`, freed with `PropVariantClear`.
- *(coreaudio)* enumerate via HAL `kAudioHardwarePropertyDevices`, keeping
  devices that expose output streams, labelling each with the
  CFString-free `kAudioDevicePropertyDeviceName`, and tagging
  `kAudioHardwarePropertyDefaultOutputDevice`.
- *(wasapi)* query real end-to-end output latency via `IAudioClock::GetPosition`
  + cached `GetFrequency`. The worker publishes
  `(frames_written - position_in_frames) / sample_rate` after each
  `ReleaseBuffer`, giving callers the live total-pipeline delay (mix
  engine + driver + device hardware) on Bluetooth, HDMI passthrough,
  and remote-desktop sinks rather than the driver-side `GetStreamLatency`
  estimate alone. Falls back to `GetStreamLatency + GetCurrentPadding`
  when the driver shim refuses the `IAudioClock` service or
  `GetFrequency` returns 0.

## [0.1.1](https://github.com/OxideAV/oxideav-sysaudio/compare/v0.1.0...v0.1.1) - 2026-04-24

### Added

- *(coreaudio)* query real HAL hardware latency for BT/USB sinks

### Other

- bump thiserror 1 → 2
- bump libloading 0.8 → 0.9
- release v0.0.1

## [0.1.0](https://github.com/OxideAV/oxideav-sysaudio/compare/v0.0.1...v0.1.0) - 2026-04-19

### Other

- bump to 0.1.0

### Added

- Initial release: pure-Rust audio output framework with runtime-loaded
  native backends via `libloading`.
- Linux: functional ALSA (`libasound.so.2`) and PulseAudio
  (`libpulse-simple.so.0`) backends. PipeWire and OSS stubbed.
- Windows: functional WASAPI backend via `ole32.dll` + `kernel32.dll`
  (COM vtables by hand, shared-mode, event-driven). ASIO stubbed.
- macOS: functional CoreAudio backend via AudioToolbox AudioQueue.
- Public API: `drivers()`, `probe()`, `default_driver()`,
  `driver_by_name()`, `open()`, `open_default()`,
  `Stream::{play, pause, format, latency, stop}`.
- `Stream::latency()` reports output-side delay per backend
  (PulseAudio end-to-end, ALSA `snd_pcm_delay`, WASAPI
  `GetStreamLatency` + padding, CoreAudio software estimate) so
  callers can compensate A/V sync on high-latency sinks like
  Bluetooth.
