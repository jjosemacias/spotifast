//! Explicit process startup only. No ENV filter, log files or panic payloads.
use std::sync::atomic::{AtomicBool, Ordering};
static ENABLED: AtomicBool = AtomicBool::new(false);
struct Silent;
impl log::Log for Silent {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        false
    }
    fn log(&self, _: &log::Record<'_>) {}
    fn flush(&self) {}
}
static SILENT: Silent = Silent;
/// Call before constructing any Session, never to sanitize an existing one.
/// Refuse an existing logger or tracing subscriber, rather than replacing it.
pub fn configure_startup() -> Result<(), &'static str> {
    if tracing::dispatcher::has_been_set()
        || tracing::dispatcher::get_default(|d| !d.is::<tracing::subscriber::NoSubscriber>())
    {
        return Err("Experimental Resume startup refused: tracing is active.");
    }
    log::set_logger(&SILENT).map_err(|_| "Experimental Resume startup refused: logger exists.")?;
    log::set_max_level(log::LevelFilter::Off);
    std::panic::set_hook(Box::new(|_| {}));
    ENABLED.store(true, Ordering::SeqCst);
    Ok(())
}
pub fn enabled() -> bool {
    ENABLED.load(Ordering::SeqCst) && crate::backend::jam::resume::privacy().is_ok()
}
