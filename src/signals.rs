use std::{sync::Arc, thread};

use dusa_collection_utils::{log, log::LogLevel};
use signal_hook::{consts::{SIGHUP, SIGUSR1}, iterator::Signals};
use tokio::sync::Notify;

pub fn reload_monitor(notify: Arc<Notify>) {
    thread::spawn(move || {
        let mut signals = Signals::new(&[SIGHUP]).expect("Failed to register signals");
        for _ in signals.forever() {
            log!(LogLevel::Info, "Received SIGHUP, reloading...");
            notify.notify_one();
        }
    });    
}

pub fn shutdown_monitor(notify: Arc<Notify>) {
    thread::spawn(move || {
        let mut signals = Signals::new(&[SIGUSR1]).expect("Failed to register signals");
        for _ in signals.forever() {
            log!(LogLevel::Info, "Received SIGHUP, exiting...");
            notify.notify_one();
        }
    });    
}