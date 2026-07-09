//! Virtual mock backend (cargo feature `mock`) — a hardware-free output
//! that renders the callback from a paced worker thread into a discard
//! (or capture) sink.
//!
//! It exists so the crate's full stream state machine — open / play /
//! pause / stop / drop, `CallbackInfo::frames_played` accounting,
//! buffer-size hints, latency reporting, per-device routing — is
//! exercisable on CI runners that have no audio hardware and no
//! loadable audio library. It is **not** part of the default feature
//! set: enabling `mock` appends a `"mock"` driver to
//! [`crate::drivers`] / [`crate::probe`], always last in the
//! preference order so a real backend still wins whenever one works.
//!
//! # Behavioural model
//!
//! - `probe()` always succeeds — the backend needs nothing from the
//!   host.
//! - Three virtual devices are enumerated: `mock:default` (tagged as
//!   the system default), `mock:secondary`, and `mock:capture`. Any
//!   other id fails `open()` with [`Error::DeviceOpen`], mirroring how
//!   real backends reject fabricated ids.
//! - `open()` honours the requested rate/channels exactly and the
//!   `buffer_frames` hint verbatim (default: `sample_rate / 50`, i.e.
//!   the crate-wide ~20 ms period). A worker thread renders one period
//!   per tick, paced at roughly the period duration, and advances the
//!   `frames_played` clock only while playing. Streams start in the
//!   playing state, like every real backend in this crate.
//! - `latency()` models a fixed two-period software queue:
//!   `Some(2 × period)`.
//! - Streams opened on `mock:capture` append every rendered sample to
//!   a global capture sink that tests drain via [`take_captured`]; the
//!   other devices discard the samples.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::backend::{Backend, Callback};
use crate::format::{CallbackInfo, Device, SampleFormat, StreamFormat, StreamRequest};
use crate::stream::StreamImpl;
use crate::{Error, Result};

pub(crate) struct MockBackend;

/// `(id, name, is_default)` for the three virtual devices.
const MOCK_DEVICES: &[(&str, &str, bool)] = &[
    ("mock:default", "Mock Default Output", true),
    ("mock:secondary", "Mock Secondary Output", false),
    ("mock:capture", "Mock Capture-Sink Output", false),
];

/// Global capture sink fed by streams opened on `"mock:capture"`.
/// Tests drain it with [`take_captured`]; growth is capped at
/// [`CAPTURE_CAP_SAMPLES`] so a forgotten stream can't eat the heap.
static CAPTURE: Mutex<Vec<f32>> = Mutex::new(Vec::new());

/// Upper bound on how many samples the capture sink retains between
/// drains (~16 MiB of f32).
const CAPTURE_CAP_SAMPLES: usize = 1 << 22;

/// Drain and return everything the `"mock:capture"` device rendered
/// since the previous drain (or since process start). The sink is
/// global — tests that assert on its contents should serialise among
/// themselves.
pub fn take_captured() -> Vec<f32> {
    std::mem::take(&mut *CAPTURE.lock().unwrap())
}

fn push_captured(chunk: &[f32]) {
    let mut sink = CAPTURE.lock().unwrap();
    let room = CAPTURE_CAP_SAMPLES.saturating_sub(sink.len());
    let take = room.min(chunk.len());
    sink.extend_from_slice(&chunk[..take]);
}

/// State shared between the worker thread and the stream handle.
struct Shared {
    playing: AtomicBool,
    shutdown: AtomicBool,
    /// Monotonic frame clock — advances by one period per rendered
    /// tick, only while playing. Read back as
    /// `CallbackInfo::frames_played`.
    frames_played: AtomicU64,
}

struct MockStream {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    fmt: StreamFormat,
    /// One rendering period — `period_frames / sample_rate`.
    period: Duration,
}

impl StreamImpl for MockStream {
    fn play(&mut self) -> Result<()> {
        self.shared.playing.store(true, Ordering::Release);
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.shared.playing.store(false, Ordering::Release);
        Ok(())
    }

    fn format(&self) -> StreamFormat {
        self.fmt
    }

    fn latency(&self) -> Option<Duration> {
        // Fixed two-period software queue model (see module docs).
        Some(self.period * 2)
    }

    fn stop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.playing.store(false, Ordering::Release);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Sleep `total`, waking early (in ≤ 10 ms slices) if the stream shuts
/// down, so `stop()` / drop never waits a full period on streams opened
/// with large buffer hints.
fn sleep_responsive(shared: &Shared, total: Duration) {
    let mut remaining = total;
    while !shared.shutdown.load(Ordering::Acquire) && remaining > Duration::ZERO {
        let slice = remaining.min(Duration::from_millis(10));
        thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

fn run_worker(
    mut cb: Callback,
    shared: Arc<Shared>,
    period_frames: usize,
    channels: usize,
    period: Duration,
    capture: bool,
) {
    let mut buf = vec![0.0f32; period_frames * channels];
    // Floor the tick so pathologically small buffer hints don't turn
    // the worker into a busy spin.
    let tick = period.max(Duration::from_micros(500));
    while !shared.shutdown.load(Ordering::Acquire) {
        if shared.playing.load(Ordering::Acquire) {
            let info = CallbackInfo {
                frames_played: shared.frames_played.load(Ordering::Relaxed),
            };
            buf.fill(0.0);
            cb(&mut buf, &info);
            if capture {
                push_captured(&buf);
            }
            shared
                .frames_played
                .fetch_add(period_frames as u64, Ordering::Release);
        }
        sleep_responsive(&shared, tick);
    }
}

/// Resolve a requested device id to one of the virtual devices.
/// `None` means the default endpoint, like every real backend.
fn resolve(id: Option<&str>) -> Result<&'static str> {
    match id {
        None => Ok("mock:default"),
        Some(want) => MOCK_DEVICES
            .iter()
            .find(|(id, _, _)| *id == want)
            .map(|(id, _, _)| *id)
            .ok_or_else(|| Error::DeviceOpen {
                backend: "mock",
                detail: format!("no such mock device: {want}"),
            }),
    }
}

impl Backend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn description(&self) -> &'static str {
        "Virtual test output — no hardware; renders to a discard/capture sink"
    }

    fn probe(&self) -> Result<()> {
        Ok(())
    }

    fn output_devices(&self) -> Result<Vec<Device>> {
        Ok(MOCK_DEVICES
            .iter()
            .map(|&(id, name, is_default)| Device {
                id: id.into(),
                name: name.into(),
                is_default,
            })
            .collect())
    }

    fn preferred_format(&self, device_id: Option<&str>) -> Result<StreamFormat> {
        resolve(device_id)?;
        Ok(StreamFormat {
            sample_rate: 48_000,
            channels: 2,
            format: SampleFormat::F32,
        })
    }

    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>> {
        // Self-defence against the period math below — a zero rate
        // would divide by zero, a zero channel count would render into
        // an empty buffer forever.
        if req.sample_rate == 0 || req.channels == 0 {
            return Err(Error::UnsupportedFormat {
                backend: "mock",
                detail: format!(
                    "sample_rate={} channels={} — both must be non-zero",
                    req.sample_rate, req.channels
                ),
            });
        }
        let dev = resolve(req.device.as_deref())?;
        let capture = dev == "mock:capture";
        let period_frames = req.buffer_frames.unwrap_or(req.sample_rate / 50).max(1) as usize;
        let period = Duration::from_secs_f64(period_frames as f64 / f64::from(req.sample_rate));
        let fmt = StreamFormat {
            sample_rate: req.sample_rate,
            channels: req.channels,
            format: SampleFormat::F32,
        };
        let shared = Arc::new(Shared {
            playing: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            frames_played: AtomicU64::new(0),
        });
        let worker_shared = shared.clone();
        let channels = usize::from(req.channels);
        let worker = thread::Builder::new()
            .name("oxideav-sysaudio-mock".into())
            .spawn(move || run_worker(cb, worker_shared, period_frames, channels, period, capture))
            .map_err(|e| Error::Runtime {
                backend: "mock",
                detail: format!("worker spawn: {e}"),
            })?;
        Ok(Box::new(MockStream {
            shared,
            worker: Some(worker),
            fmt,
            period,
        }))
    }
}
