//! magic-mixer: command-line backend for the Magic Mixer.
//!
//!   magic-mixer apps                         list apps with audio sessions
//!   magic-mixer devices                      list playback + recording devices
//!   magic-mixer route <app> <output> [gain%] patch an app into an output device
//!   magic-mixer meter <input> [seconds]      show the level arriving at a recording device

mod com;
mod outputs;
mod ring;
mod routes;
mod sources;

use std::io::BufRead;

use outputs::Direction;

fn usage() -> ! {
    eprintln!(
        "usage:
  magic-mixer apps
  magic-mixer devices
  magic-mixer route <app name or pid> <output device name|default> [gain%]
  magic-mixer meter <device name|default> [seconds]
  magic-mixer default <device name>          make a device the Windows default

examples:
  magic-mixer route spotify \"CABLE Input\" 80
  magic-mixer meter \"CABLE Output\" 5"
    );
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let _com = com::ComGuard::new().expect("COM init failed");
    let result = match args.first().map(String::as_str) {
        Some("apps") => cmd_apps(),
        Some("devices") => cmd_devices(),
        Some("route") if args.len() >= 3 => cmd_route(&args[1], &args[2], args.get(3)),
        Some("meter") if args.len() >= 2 => cmd_meter(&args[1], args.get(2)),
        Some("default") if args.len() >= 2 => cmd_default(&args[1]),
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

type Res = Result<(), Box<dyn std::error::Error>>;

fn bar(level: f32) -> String {
    let n = (level.clamp(0.0, 1.0) * 20.0).round() as usize;
    format!("{}{}", "#".repeat(n), ".".repeat(20 - n))
}

fn cmd_apps() -> Res {
    let apps = sources::list_apps()?;
    if apps.is_empty() {
        println!("No apps have audio open right now.");
    }
    println!("{:>7}  {:<8} {:<22} {}", "PID", "STATE", "LEVEL", "APP");
    for a in apps {
        let state = if a.active { "playing" } else { "idle" };
        println!("{:>7}  {:<8} {:<22} {}", a.pid, state, bar(a.peak), a.name);
    }
    Ok(())
}

fn cmd_devices() -> Res {
    for (title, dir) in [("Playback (route outputs)", Direction::Output), ("Recording", Direction::Input)] {
        println!("{title}:");
        for d in outputs::list_devices(dir)? {
            println!("  {} {}", if d.is_default { "*" } else { " " }, d.name);
        }
    }
    println!("(* = Windows default)");
    Ok(())
}

fn parse_gain(s: &str) -> Option<f32> {
    s.trim().trim_end_matches('%').parse::<f32>().ok().map(|p| p / 100.0)
}

fn cmd_route(app: &str, out: &str, gain: Option<&String>) -> Res {
    let source = sources::find_app(app)?.ok_or_else(|| format!("no app matching '{app}' has audio open (try `magic-mixer apps`)"))?;
    let output = outputs::find_device(out, Direction::Output)?
        .ok_or_else(|| format!("no playback device matching '{out}' (try `magic-mixer devices`)"))?;
    let gain = gain.and_then(|g| parse_gain(g)).unwrap_or(1.0);

    println!("Routing {} (pid {}) -> {} at {:.0}%", source.name, source.pid, output.name, gain * 100.0);
    println!("Type a number (0-400) for gain %, 'm' to mute/unmute, 'q' to stop.");
    let route = routes::Route::start(source, output, gain);

    // Watch for a route thread dying so errors surface even if nobody types.
    let stdin = std::io::stdin();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in stdin.lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut last_status = std::time::Instant::now();
    loop {
        if route.has_failed() {
            break;
        }
        if last_status.elapsed() >= std::time::Duration::from_secs(2) {
            last_status = std::time::Instant::now();
            let p = route.gain.take_peak();
            println!("level {}  {:.3}{}", bar(p), p, if route.gain.is_muted() { "  (muted)" } else { "" });
        }
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(line) => match line.trim_start_matches('\u{feff}').trim() {
                "q" | "quit" => break,
                "m" => {
                    let muted = route.gain.toggle_mute();
                    println!("{}", if muted { "muted" } else { "unmuted" });
                }
                other => match parse_gain(other) {
                    Some(g) => {
                        route.gain.set(g);
                        println!("gain {:.0}%", route.gain.get() * 100.0);
                    }
                    None => println!("?  (number = gain %, m = mute, q = quit)"),
                },
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // stdin closed (e.g. run non-interactively): keep routing until a thread exits.
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
    let errors = route.stop();
    if errors.is_empty() {
        println!("Route stopped.");
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}

fn cmd_default(dev: &str) -> Res {
    // Search playback first, then recording; an exact name match in either wins.
    let mut found = None;
    for dir in [Direction::Output, Direction::Input] {
        if let Some(d) = outputs::find_device(dev, dir)? {
            if d.name.eq_ignore_ascii_case(dev) || found.is_none() {
                found = Some(d);
            }
        }
    }
    let d = found.ok_or_else(|| format!("no device matching '{dev}'"))?;
    outputs::set_default_device(&d.id)?;
    println!("Default device is now: {}", d.name);
    Ok(())
}

fn cmd_meter(dev: &str, secs: Option<&String>) -> Res {
    // Recording devices are metered directly; playback devices via loopback
    // (i.e. "what is being played into this device").
    let (d, loopback) = match outputs::find_device(dev, Direction::Input)? {
        Some(d) => (d, false),
        None => (
            outputs::find_device(dev, Direction::Output)?.ok_or_else(|| format!("no device matching '{dev}'"))?,
            true,
        ),
    };
    let secs = secs.and_then(|s| s.parse().ok()).unwrap_or(3.0f32);
    let how = if loopback { " (playback, via loopback)" } else { "" };
    println!("Listening to {}{how} for {secs}s...", d.name);
    let (peak, rms) = outputs::meter_input(&d.id, loopback, secs)?;
    let db = |x: f32| if x > 0.0 { 20.0 * x.log10() } else { f32::NEG_INFINITY };
    println!("peak {:.3} ({:.1} dBFS)  rms {:.4} ({:.1} dBFS)", peak, db(peak), rms, db(rms));
    Ok(())
}
