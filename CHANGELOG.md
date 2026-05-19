# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
