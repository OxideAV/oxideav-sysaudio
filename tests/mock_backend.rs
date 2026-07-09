//! State-machine coverage driven through the virtual `mock` backend
//! (cargo feature `mock`). Everything in this file runs on a
//! hardware-free CI runner: the mock backend needs no audio library
//! and no device, so these tests exercise the crate's full public
//! surface — probing, enumeration, per-device routing, the frame
//! clock, pause/resume, stop/drop teardown, latency reporting —
//! deterministically.
#![cfg(feature = "mock")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use oxideav_sysaudio::{
    driver_by_name, drivers, mock, open, open_on, probe, Device, Driver, Error, SampleFormat,
    StreamRequest,
};

fn mock_driver() -> Driver {
    driver_by_name("mock").expect("mock backend is compiled in under --features mock")
}

/// Serialises the tests that read the global capture sink so they
/// don't drain each other's samples when the harness runs in parallel.
fn capture_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Poll `cond` every 2 ms until it holds or `deadline` elapses;
/// returns the final evaluation.
fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
}

const DEADLINE: Duration = Duration::from_secs(5);

#[test]
fn mock_is_registered_last_and_probes() {
    // Preference contract: the mock never shadows a real backend, so
    // it must be the last entry of drivers() on every platform.
    let all = drivers();
    assert_eq!(all.last().map(|d| d.name()), Some("mock"));
    // probe() always accepts it (no hardware needed) — this is what
    // makes the rest of this file runnable on a headless CI box.
    assert!(probe().iter().any(|d| d.name() == "mock"));
    assert!(!mock_driver().is_stub());
}

#[test]
fn enumeration_lists_three_devices_with_one_default() {
    let devs = mock_driver().output_devices().expect("mock enumerates");
    assert_eq!(devs.len(), 3);
    assert_eq!(devs.iter().filter(|d| d.is_default).count(), 1);
    let def = mock_driver()
        .default_output_device()
        .expect("no backend error")
        .expect("mock has a default device");
    assert_eq!(def.id, "mock:default");
    assert!(def.is_default);
    // The one-call shortcut and the enumeration must agree.
    assert_eq!(devs.into_iter().find(|d| d.is_default), Some(def));
}

#[test]
fn preferred_format_reports_and_rejects_alien_ids() {
    let fmt = mock_driver()
        .preferred_format(None)
        .expect("no backend error")
        .expect("mock introspects");
    assert_eq!(fmt.sample_rate, 48_000);
    assert_eq!(fmt.channels, 2);
    assert_eq!(fmt.format, SampleFormat::F32);
    let alien = Device {
        id: "mock:no-such-device".into(),
        name: "fake".into(),
        is_default: false,
    };
    assert!(
        mock_driver().preferred_format(Some(&alien)).is_err(),
        "a fabricated id must not produce a fabricated format"
    );
}

#[test]
fn stream_starts_playing_with_hinted_buffers_and_monotonic_clock() {
    // Codifies two contracts at once:
    //  1. streams start in the playing state — no play() call below,
    //     yet callbacks arrive (all real backends behave this way:
    //     their `paused` flag initialises to false at open());
    //  2. the buffer hint is honoured verbatim and
    //     `CallbackInfo::frames_played` advances by exactly one period
    //     per callback, starting at zero.
    let seen: Arc<Mutex<Vec<(usize, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_cb = seen.clone();
    let req = StreamRequest::new(48_000, 2).with_buffer_frames(Some(128));
    let stream = open(mock_driver(), req, move |out, info| {
        seen_cb
            .lock()
            .unwrap()
            .push((out.len(), info.frames_played));
    })
    .expect("mock open");
    assert_eq!(stream.format().sample_rate, 48_000);
    assert_eq!(stream.format().channels, 2);
    assert_eq!(stream.format().format, SampleFormat::F32);
    assert!(
        wait_until(DEADLINE, || seen.lock().unwrap().len() >= 4),
        "no callbacks arrived — stream did not start playing at open()"
    );
    let snap = seen.lock().unwrap().clone();
    for (len, _) in &snap {
        assert_eq!(*len, 128 * 2, "buffer length must be hint × channels");
    }
    assert_eq!(snap[0].1, 0, "frame clock must start at zero");
    for w in snap.windows(2) {
        assert_eq!(
            w[1].1,
            w[0].1 + 128,
            "frame clock must advance by one period per callback"
        );
    }
    drop(stream);
}

#[test]
fn pause_halts_the_clock_and_play_resumes_it() {
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    let req = StreamRequest::new(48_000, 1).with_buffer_frames(Some(64));
    let mut stream = open(mock_driver(), req, move |_, _| {
        t.fetch_add(1, Ordering::Relaxed);
    })
    .expect("mock open");
    assert!(wait_until(DEADLINE, || ticks.load(Ordering::Relaxed) >= 3));
    stream.pause().expect("mock pause");
    // One render may be in flight when pause() lands; wait for the
    // counter to go quiet before asserting it stays quiet.
    let mut last = ticks.load(Ordering::Relaxed);
    assert!(
        wait_until(DEADLINE, || {
            std::thread::sleep(Duration::from_millis(30));
            let now = ticks.load(Ordering::Relaxed);
            let stable = now == last;
            last = now;
            stable
        }),
        "callback counter never settled after pause()"
    );
    let frozen = ticks.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(
        ticks.load(Ordering::Relaxed),
        frozen,
        "callback ran while paused"
    );
    stream.play().expect("mock play");
    assert!(
        wait_until(DEADLINE, || ticks.load(Ordering::Relaxed) > frozen),
        "callbacks did not resume after play()"
    );
}

#[test]
fn stop_is_clean_and_drop_joins_the_worker() {
    // stop() consumes the handle and must return promptly (the worker
    // sleeps in ≤ 10 ms slices even with a 1-second buffer hint).
    let big = StreamRequest::new(48_000, 1).with_buffer_frames(Some(48_000));
    let s = open(mock_driver(), big, |_, _| {}).expect("mock open");
    let t0 = Instant::now();
    s.stop();
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "stop() blocked on a full period of a large buffer"
    );

    // Drop performs the same teardown; after it returns the worker is
    // joined, so the callback counter can never move again.
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    let s2 = open(
        mock_driver(),
        StreamRequest::new(48_000, 1).with_buffer_frames(Some(64)),
        move |_, _| {
            t.fetch_add(1, Ordering::Relaxed);
        },
    )
    .expect("mock open");
    assert!(wait_until(DEADLINE, || ticks.load(Ordering::Relaxed) >= 1));
    drop(s2);
    let frozen = ticks.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(
        ticks.load(Ordering::Relaxed),
        frozen,
        "callback ran after drop returned — worker not joined"
    );
}

#[test]
fn per_device_routing_accepts_known_and_rejects_fabricated_ids() {
    let devs = mock_driver().output_devices().expect("mock enumerates");
    let secondary = devs
        .iter()
        .find(|d| d.id == "mock:secondary")
        .expect("secondary device listed");
    let s = open_on(
        mock_driver(),
        secondary,
        StreamRequest::new(44_100, 2),
        |_, _| {},
    )
    .expect("open on a real enumerated id");
    assert_eq!(s.format().sample_rate, 44_100);
    drop(s);

    let fake = Device {
        id: "not-a-mock-device".into(),
        name: "fake".into(),
        is_default: false,
    };
    let err = open_on(
        mock_driver(),
        &fake,
        StreamRequest::new(44_100, 2),
        |_, _| {},
    )
    .expect_err("fabricated id must not open");
    assert!(
        matches!(
            err,
            Error::DeviceOpen {
                backend: "mock",
                ..
            }
        ),
        "expected DeviceOpen, got {err}"
    );
}

#[test]
fn latency_models_two_periods() {
    let req = StreamRequest::new(48_000, 2).with_buffer_frames(Some(240)); // 5 ms period
    let s = open(mock_driver(), req, |_, _| {}).expect("mock open");
    let lat = s.latency().expect("mock reports latency");
    let expect = Duration::from_secs_f64(2.0 * 240.0 / 48_000.0);
    let diff = if lat > expect {
        lat - expect
    } else {
        expect - lat
    };
    assert!(
        diff < Duration::from_micros(100),
        "latency {lat:?}, expected ~{expect:?}"
    );
}

#[test]
fn is_playing_tracks_transport_requests() {
    let mut s = open(mock_driver(), StreamRequest::new(8_000, 1), |_, _| {}).expect("mock open");
    assert!(s.is_playing(), "streams start in the playing state");
    s.pause().expect("mock pause");
    assert!(!s.is_playing());
    s.play().expect("mock play");
    assert!(s.is_playing());
}

#[test]
fn volume_getter_defaults_and_clamps() {
    let s = open(mock_driver(), StreamRequest::new(8_000, 1), |_, _| {}).expect("mock open");
    assert_eq!(s.volume(), 1.0, "default volume is unity gain");
    s.set_volume(0.25);
    assert_eq!(s.volume(), 0.25);
    s.set_volume(2.0);
    assert_eq!(s.volume(), 2.0, "amplification above 1.0 is allowed");
    s.set_volume(-3.0);
    assert_eq!(s.volume(), 0.0, "negative volume clamps to silence");
    s.set_volume(f32::NAN);
    assert_eq!(s.volume(), 0.0, "NaN clamps to silence");
}

/// Open a capture stream whose callback writes `rendered` everywhere,
/// set `volume`, then wait until a freshly drained chunk consists
/// entirely of `expect` — i.e. the gain change has propagated to the
/// audio thread. Panics on deadline.
fn assert_capture_converges(rendered: f32, volume: f32, expect: f32) {
    let _guard = capture_lock();
    let _ = mock::take_captured(); // drain leftovers
    let devs = mock_driver().output_devices().expect("mock enumerates");
    let cap = devs
        .iter()
        .find(|d| d.id == "mock:capture")
        .expect("capture device listed");
    let req = StreamRequest::new(48_000, 1).with_buffer_frames(Some(64));
    let s = open_on(mock_driver(), cap, req, move |out, _| out.fill(rendered))
        .expect("mock open on capture");
    s.set_volume(volume);
    // Chunks rendered before set_volume() landed may carry the old
    // gain; converge on "an entire freshly-drained, non-empty chunk is
    // at the new value".
    let ok = wait_until(DEADLINE, || {
        let chunk = mock::take_captured();
        !chunk.is_empty() && chunk.iter().all(|&x| x == expect)
    });
    drop(s);
    assert!(
        ok,
        "capture never converged: cb wrote {rendered}, volume {volume}, expected {expect}"
    );
}

#[test]
fn volume_scales_rendered_samples() {
    // 0.5 × 0.5 is exact in f32, so equality is safe.
    assert_capture_converges(0.5, 0.5, 0.25);
}

#[test]
fn volume_zero_silences_output() {
    assert_capture_converges(1.0, 0.0, 0.0);
}

#[test]
fn capture_sink_sees_exactly_what_the_callback_rendered() {
    let _guard = capture_lock();
    let _ = mock::take_captured(); // drain leftovers from other streams
    let devs = mock_driver().output_devices().expect("mock enumerates");
    let cap = devs
        .iter()
        .find(|d| d.id == "mock:capture")
        .expect("capture device listed");
    let req = StreamRequest::new(48_000, 1).with_buffer_frames(Some(64));
    let s = open_on(mock_driver(), cap, req, |out, _| out.fill(0.5)).expect("mock open");
    let mut got: Vec<f32> = Vec::new();
    assert!(
        wait_until(DEADLINE, || {
            got.extend(mock::take_captured());
            got.len() >= 256
        }),
        "capture sink stayed empty"
    );
    drop(s);
    assert!(
        got.iter().all(|&x| x == 0.5),
        "captured samples differ from what the callback wrote"
    );
}
