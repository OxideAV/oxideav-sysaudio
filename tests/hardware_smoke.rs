//! Real-hardware lifecycle smoke test.
//!
//! Drives one full open → play → latency → pause → resume → drop cycle
//! through the first *real* (non-mock) backend that passes `probe()`,
//! rendering silence. On a headless CI runner no real backend probes,
//! so the test degrades to a clean, loud skip — it must never fail
//! merely because the host has no audio stack.
//!
//! Assertion policy: pure-software invariants (sane negotiated format,
//! no panic/hang in the transport calls, teardown returns) are always
//! asserted. Environment-dependent observations (callbacks actually
//! arriving, latency being reported) are asserted only under
//! `OXIDEAV_SYSAUDIO_STRICT_HW=1` — set that locally on a machine with
//! a known-good output device; CI boxes occasionally expose a device
//! that opens but never renders, which must not turn the matrix red.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

#[test]
fn real_backend_lifecycle_smoke() {
    let strict = std::env::var_os("OXIDEAV_SYSAUDIO_STRICT_HW").is_some();
    let Some(driver) = oxideav_sysaudio::probe()
        .into_iter()
        .find(|d| d.name() != "mock")
    else {
        eprintln!("SKIP: no real audio backend probed on this host (headless runner)");
        assert!(
            !strict,
            "OXIDEAV_SYSAUDIO_STRICT_HW is set but no real backend probed"
        );
        return;
    };
    eprintln!("hardware smoke: driver = {}", driver.name());
    assert!(driver.status().is_ready(), "probed driver must be Ready");

    // Enumeration invariants on real hardware (headless CI never gets
    // here, so these run against genuine OS answers).
    match driver.output_devices() {
        Ok(devs) => {
            assert!(
                devs.iter().filter(|d| d.is_default).count() <= 1,
                "more than one default device"
            );
            for d in &devs {
                eprintln!(
                    "  device: {}{} [{}]",
                    d.name,
                    if d.is_default { " (default)" } else { "" },
                    d.id
                );
            }
        }
        Err(e) => eprintln!("  output_devices: {e}"),
    }
    if let Ok(Some(fmt)) = driver.preferred_format(None) {
        eprintln!("  preferred: {} Hz, {} ch", fmt.sample_rate, fmt.channels);
        assert!(fmt.sample_rate > 0);
        assert!(fmt.channels > 0);
    }

    let callbacks = Arc::new(AtomicU64::new(0));
    let cb_count = callbacks.clone();
    let req = oxideav_sysaudio::StreamRequest::new(48_000, 2);
    let mut stream = match oxideav_sysaudio::open(driver, req, move |out, _info| {
        out.fill(0.0); // silence — this test may run on someone's desk
        cb_count.fetch_add(1, Ordering::Relaxed);
    }) {
        Ok(s) => s,
        Err(e) => {
            // probe() passed a moment ago, so this is unusual but can
            // legitimately happen (device claimed exclusively between
            // the two calls). Only strict mode treats it as fatal.
            if strict {
                panic!("open() failed on a Ready backend: {e}");
            }
            eprintln!("SKIP: open() failed on a Ready backend: {e}");
            return;
        }
    };

    // Software invariants — always asserted.
    let fmt = stream.format();
    assert!(fmt.sample_rate > 0, "negotiated rate must be non-zero");
    assert!(fmt.channels > 0, "negotiated channels must be non-zero");
    assert!(stream.is_playing(), "streams start playing at open()");

    let rendered = wait_until(Duration::from_secs(5), || {
        callbacks.load(Ordering::Relaxed) > 0
    });
    if strict {
        assert!(rendered, "no callbacks within 5 s on {}", driver.name());
    } else if !rendered {
        eprintln!("WARN: stream opened but no callbacks arrived within 5 s");
    }

    match stream.latency() {
        Some(l) => {
            eprintln!("  latency: {l:?}");
            assert!(
                l < Duration::from_secs(10),
                "reported latency is absurd: {l:?}"
            );
        }
        None => {
            if strict {
                panic!("{}: latency() returned None", driver.name());
            }
            eprintln!("  latency: not reported");
        }
    }

    // Transport round-trip must not error, panic, or hang.
    stream.pause().expect("pause on real backend");
    assert!(!stream.is_playing());
    stream.play().expect("resume on real backend");
    assert!(stream.is_playing());

    // Volume calls are pure software (atomic store) even on a real
    // stream; exercise them on the live audio thread.
    stream.set_volume(0.0);
    assert_eq!(stream.volume(), 0.0);
    stream.set_volume(1.0);

    // Teardown must return promptly.
    let t0 = Instant::now();
    drop(stream);
    let took = t0.elapsed();
    eprintln!("  teardown: {took:?}");
    assert!(
        took < Duration::from_secs(5),
        "drop() took {took:?} — teardown hang"
    );
}
