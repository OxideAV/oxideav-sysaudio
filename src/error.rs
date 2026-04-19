//! Error type shared across all backends.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The selected backend is compiled in as a stub only.
    NotImplemented(&'static str),
    /// The backend's shared library failed to load (wrong soname, not
    /// installed, permission denied). `probe()` swallows this; `open()`
    /// surfaces it.
    LibraryLoad {
        backend: &'static str,
        soname: &'static str,
        source: libloading::Error,
    },
    /// A specific symbol was missing from an otherwise-loaded library.
    SymbolMissing {
        backend: &'static str,
        symbol: &'static str,
        source: libloading::Error,
    },
    /// The shared library loaded but opening a device/stream failed.
    /// `detail` is backend-specific (ALSA error string, HRESULT, …).
    DeviceOpen {
        backend: &'static str,
        detail: String,
    },
    /// The request (sample rate, channels, format) couldn't be satisfied
    /// by the selected device.
    UnsupportedFormat {
        backend: &'static str,
        detail: String,
    },
    /// Caller asked for `open_default()` or `open(driver_by_name)` and
    /// no matching driver is available.
    NoDriver(String),
    /// Catch-all for backend runtime failures after the stream is open
    /// (write errors, worker-thread crashes, etc.).
    Runtime {
        backend: &'static str,
        detail: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotImplemented(b) => write!(f, "{b}: backend not yet implemented"),
            Error::LibraryLoad {
                backend,
                soname,
                source,
            } => {
                write!(f, "{backend}: loading {soname}: {source}")
            }
            Error::SymbolMissing {
                backend,
                symbol,
                source,
            } => {
                write!(f, "{backend}: symbol {symbol} missing: {source}")
            }
            Error::DeviceOpen { backend, detail } => write!(f, "{backend}: open: {detail}"),
            Error::UnsupportedFormat { backend, detail } => {
                write!(f, "{backend}: unsupported format: {detail}")
            }
            Error::NoDriver(s) => write!(f, "no audio driver available: {s}"),
            Error::Runtime { backend, detail } => write!(f, "{backend}: runtime: {detail}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::LibraryLoad { source, .. } | Error::SymbolMissing { source, .. } => Some(source),
            _ => None,
        }
    }
}
