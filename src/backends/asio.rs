//! ASIO output backend — stub.
//!
//! ASIO drivers are vendor-supplied DLLs registered under
//! `HKLM\SOFTWARE\ASIO`. A functional implementation walks the
//! registry, loads the vendor DLL, and talks to its COM-flavoured
//! interface. Out of scope for the initial landing.

use crate::backend::{Backend, Callback};
use crate::format::StreamRequest;
use crate::stream::StreamImpl;
use crate::{Error, Result};

pub(crate) struct AsioBackend;

impl Backend for AsioBackend {
    fn name(&self) -> &'static str {
        "asio"
    }
    fn description(&self) -> &'static str {
        "ASIO (stub — not yet implemented)"
    }
    fn is_stub(&self) -> bool {
        true
    }
    fn probe(&self) -> Result<()> {
        Err(Error::NotImplemented("asio"))
    }
    fn open(&self, _req: StreamRequest, _cb: Callback) -> Result<Box<dyn StreamImpl>> {
        Err(Error::NotImplemented("asio"))
    }
}
