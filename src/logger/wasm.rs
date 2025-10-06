#![cfg(target_arch = "wasm32")]

use log::{LevelFilter, Log, Metadata, Record};
use web_sys::console;

struct WasmLogger;

impl Log for WasmLogger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            console::log_1(&format!("[WASM] {}", record.args()).into());
        }
    }

    fn flush(&self) {}
}

static LOGGER: WasmLogger = WasmLogger;

pub fn init_logger() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(LevelFilter::Trace);
        log::info!("WASM Logger initialized.");
    } else {
        log::set_max_level(LevelFilter::Trace);
    }
}
