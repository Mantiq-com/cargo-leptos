//! cargo-leptos prints its `tracing` events through `flexi_logger`, a `log` logger, and the
//! build watchers wait for lines such as "Finished generating JS/WASM". A dependency that sets
//! a `tracing` dispatcher, even an empty one, must not silence those lines. This file is its
//! own test binary, so the logger and the dispatcher stay local to it.

use flexi_logger::{FileSpec, Logger, WriteMode};

#[test]
fn events_reach_the_logger_after_a_dependency_sets_a_dispatcher() {
    let dir = temp_dir::TempDir::new().expect("create a temporary directory");
    let _logger = Logger::try_with_str("info")
        .expect("a valid log specification")
        .log_to_file(
            FileSpec::default()
                .directory(dir.path())
                .basename("cargo-leptos")
                .suppress_timestamp(),
        )
        .write_mode(WriteMode::Direct)
        .start()
        .expect("no logger was installed before");
    tracing::info!("Front splitting out lazy-loaded WASM files");

    // A dependency that hands its worker threads the caller's empty dispatcher.
    std::mem::forget(tracing::dispatcher::set_default(&tracing::Dispatch::none()));

    std::thread::spawn(|| tracing::info!("Finished generating JS/WASM"))
        .join()
        .expect("the logging thread finishes");

    let log =
        std::fs::read_to_string(dir.path().join("cargo-leptos.log")).expect("read the log file");
    assert!(
        log.contains("Finished generating JS/WASM"),
        "the event did not reach the logger: {log:?}"
    );
}
