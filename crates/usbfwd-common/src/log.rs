//! A logger in sixty lines.
//!
//! Both binaries run under systemd, which timestamps and routes stderr
//! already, so all that is actually needed is a level filter and a prefix.
//! Pulling in `log` + `env_logger` for that would cost a dozen crates and the
//! ability to build a fully static binary without thinking about it.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Where log lines go. Stderr by default; Android redirects this to logcat,
/// since an app's stderr goes nowhere.
static SINK: AtomicUsize = AtomicUsize::new(0);

/// Install a custom sink. The function pointer is stored as a `usize` so the
/// whole logger stays lock-free on the write path.
pub fn set_sink(f: fn(Level, &str)) {
    SINK.store(f as usize, Ordering::Relaxed);
}

pub fn set_level(l: Level) {
    LEVEL.store(l as u8, Ordering::Relaxed);
}

pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

pub fn enabled(l: Level) -> bool {
    (l as u8) <= level()
}

/// Honour `USBFWD_LOG=debug` if the caller has not set a level explicitly.
pub fn from_env() {
    if let Ok(v) = std::env::var("USBFWD_LOG") {
        if let Some(l) = Level::parse(&v) {
            set_level(l);
        }
    }
}

#[doc(hidden)]
pub fn emit(l: Level, args: std::fmt::Arguments<'_>) {
    let sink = SINK.load(Ordering::Relaxed);
    if sink != 0 {
        // Safety: only ever set from `set_sink`, which takes a real fn pointer.
        let f: fn(Level, &str) = unsafe { std::mem::transmute(sink) };
        f(l, &args.to_string());
        return;
    }
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "[{}] {}", l.tag(), args);
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $($arg:tt)*) => {{
        if $crate::log::enabled($lvl) {
            $crate::log::emit($lvl, format_args!($($arg)*));
        }
    }};
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Error, $($arg)*) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Warn, $($arg)*) };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Info, $($arg)*) };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Debug, $($arg)*) };
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Trace, $($arg)*) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_ordered_and_parseable() {
        assert!(Level::Error < Level::Warn);
        assert_eq!(Level::parse("DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("nonsense"), None);
    }

    #[test]
    fn a_custom_sink_receives_formatted_lines() {
        use std::sync::atomic::AtomicU32;
        static HITS: AtomicU32 = AtomicU32::new(0);
        fn sink(l: Level, msg: &str) {
            assert_eq!(l, Level::Error);
            assert_eq!(msg, "boom 42");
            HITS.fetch_add(1, Ordering::SeqCst);
        }
        set_sink(sink);
        crate::error!("boom {}", 42);
        assert_eq!(HITS.load(Ordering::SeqCst), 1);
        SINK.store(0, Ordering::Relaxed);
    }

    #[test]
    fn filtering_follows_the_configured_level() {
        set_level(Level::Warn);
        assert!(enabled(Level::Error));
        assert!(enabled(Level::Warn));
        assert!(!enabled(Level::Info));
        set_level(Level::Info);
    }
}
