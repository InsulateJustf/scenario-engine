mod openwrt;

use std::process;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use luyaz_scenario::Engine;
use openwrt::OpenWrtIo;

fn main() {
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--release-scan") {
        if let Err(error) = openwrt::release_scan_device() {
            eprintln!("scenario-engine: scan cleanup failed: {error}");
            process::exit(1);
        }
        return;
    }
    let io = Arc::new(OpenWrtIo);
    let engine = Arc::new(Engine::new(io));
    eprintln!("scenario-engine: starting OpenWrt scenario engine");

    let engine_thread = {
        let engine = Arc::clone(&engine);
        thread::Builder::new()
            .name("scenario".to_string())
            .spawn(move || engine.run(Duration::from_secs(600)))
    };
    let engine_thread = match engine_thread {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("scenario-engine: could not start engine thread: {error}");
            process::exit(1);
        }
    };

    loop {
        thread::sleep(Duration::from_secs(30));
        if engine.since_beat().is_some_and(|age| age > luyaz_scenario::WEDGED) {
            eprintln!("scenario-engine: engine heartbeat wedged; exiting for procd restart");
            process::exit(1);
        }
        if engine_thread.is_finished() {
            eprintln!("scenario-engine: engine thread exited; exiting for procd restart");
            process::exit(1);
        }
    }
}
