#![cfg(feature = "jam-host")]
// Separate process so configuring the logger never contaminates other tests.
#[test]
fn silent_startup_ignores_synthetic_env_and_suppresses_panic_payloads() {
    for child in [
        "startup_child",
        "startup_existing_logger",
        "startup_existing_tracing",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", child, "--ignored", "--nocapture"])
            .env("RUST_LOG", "trace")
            .output()
            .unwrap();
        assert!(output.status.success(), "{child}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "{child} executed"
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-private-panic"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-private-panic"));
    }
}
#[test]
#[ignore = "invoked in an isolated child by the startup test"]
fn startup_child() {
    spotifast::experimental_resume::configure_startup().unwrap();
    assert!(spotifast::experimental_resume::enabled());
    assert_eq!(log::max_level(), log::LevelFilter::Off);
    assert!(!tracing::dispatcher::has_been_set());
    assert!(std::panic::catch_unwind(|| panic!("synthetic-private-panic")).is_err());
    assert!(spotifast::experimental_resume::configure_startup().is_err());
}

#[test]
#[ignore = "isolated child"]
fn startup_existing_logger() {
    struct Existing;
    impl log::Log for Existing {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            false
        }
        fn log(&self, _: &log::Record<'_>) {}
        fn flush(&self) {}
    }
    static EXISTING: Existing = Existing;
    log::set_logger(&EXISTING).unwrap();
    log::set_max_level(log::LevelFilter::Info);
    assert!(spotifast::experimental_resume::configure_startup().is_err());
    assert_eq!(log::max_level(), log::LevelFilter::Info);
    assert!(!spotifast::experimental_resume::enabled());
}
#[test]
#[ignore = "isolated child"]
fn startup_existing_tracing() {
    tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default()).unwrap();
    assert!(spotifast::experimental_resume::configure_startup().is_err());
    assert!(!spotifast::experimental_resume::enabled());
}
