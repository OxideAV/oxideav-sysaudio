//! PipeWire output backend — stub.
//!
//! A functional version would dlopen `libpipewire-0.3.so.0` and drive
//! a stream through `pw_main_loop` / `pw_stream`. That's a sizeable
//! chunk of code and is tracked as a follow-up; for now `probe()`
//! fails so auto-selection falls through to PulseAudio.

use crate::backend::{Backend, Callback};
use crate::format::StreamRequest;
use crate::stream::StreamImpl;
use crate::{Error, Result};

pub(crate) struct PipeWireBackend;

impl Backend for PipeWireBackend {
    fn name(&self) -> &'static str {
        "pipewire"
    }
    fn description(&self) -> &'static str {
        "PipeWire (stub — not yet implemented)"
    }
    fn probe(&self) -> Result<()> {
        Err(Error::NotImplemented("pipewire"))
    }
    fn open(&self, _req: StreamRequest, _cb: Callback) -> Result<Box<dyn StreamImpl>> {
        Err(Error::NotImplemented("pipewire"))
    }
}
