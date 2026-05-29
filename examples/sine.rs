// Smoke test — play a 440 Hz sine for 1.5 s via probe()'s top pick.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    println!("Available drivers:");
    for d in oxideav_sysaudio::drivers() {
        println!("  - {} ({})", d.name(), d.description());
    }

    println!("\nProbe:");
    let probed = oxideav_sysaudio::probe();
    for d in &probed {
        println!("  - {}", d.name());
    }
    if probed.is_empty() {
        eprintln!("no drivers probed successfully");
        std::process::exit(1);
    }

    println!("\nOutput devices (top driver):");
    match probed[0].output_devices() {
        Ok(devs) if !devs.is_empty() => {
            for dev in devs {
                let tag = if dev.is_default { " (default)" } else { "" };
                println!("  - {}{tag}  [{}]", dev.name, dev.id);
            }
        }
        Ok(_) => println!("  (backend lists no devices — default only)"),
        Err(e) => println!("  (enumeration failed: {e})"),
    }

    let sample_rate = 48_000u32;
    let channels = 2u16;
    // If `OXIDEAV_SYSAUDIO_DEVICE` is set, route to that enumerated id;
    // otherwise fall back to the system default. The id format depends on
    // the active backend — see crate README "Opening a specific device".
    let device_override = std::env::var("OXIDEAV_SYSAUDIO_DEVICE").ok();
    let req = match device_override.as_deref() {
        Some(id) => {
            println!("\nRouting to device id: {id}");
            oxideav_sysaudio::StreamRequest::new(sample_rate, channels).with_device(id)
        }
        None => oxideav_sysaudio::StreamRequest::new(sample_rate, channels),
    };

    let phase = Arc::new(AtomicU64::new(0));
    let phase_cb = phase.clone();
    let mut stream = oxideav_sysaudio::open_default(req, move |out, _info| {
        let ch = channels as usize;
        let frames = out.len() / ch;
        let mut p = phase_cb.load(Ordering::Relaxed) as f32;
        let step = 440.0 * 2.0 * std::f32::consts::PI / sample_rate as f32;
        for f in 0..frames {
            let v = (p).sin() * 0.1;
            for c in 0..ch {
                out[f * ch + c] = v;
            }
            p += step;
            if p > std::f32::consts::TAU {
                p -= std::f32::consts::TAU;
            }
        }
        phase_cb.store(p as u64, Ordering::Relaxed);
    })
    .expect("open failed");

    println!("\nStreaming on: {}", stream.format().sample_rate);
    stream.play().ok();
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(250));
        match stream.latency() {
            Some(l) => println!("  latency: {} us ({:?})", l.as_micros(), l),
            None => println!("  latency: (not reported)"),
        }
    }
    println!("done");
}
