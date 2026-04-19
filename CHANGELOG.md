# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
