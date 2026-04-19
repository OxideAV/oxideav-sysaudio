//! OSS output backend — stub.
//!
//! Functional OSS support requires opening `/dev/dsp` and calling
//! `SNDCTL_DSP_*` ioctls; tracked as a follow-up. OSS is largely
//! supplanted by PulseAudio/ALSA on Linux, so it sits last in the
//! probe order.

use crate::backend::{Backend, Callback};
use crate::format::StreamRequest;
use crate::stream::StreamImpl;
use crate::{Error, Result};

pub(crate) struct OssBackend;

impl Backend for OssBackend {
    fn name(&self) -> &'static str {
        "oss"
    }
    fn description(&self) -> &'static str {
        "OSS / /dev/dsp (stub — not yet implemented)"
    }
    fn probe(&self) -> Result<()> {
        Err(Error::NotImplemented("oss"))
    }
    fn open(&self, _req: StreamRequest, _cb: Callback) -> Result<Box<dyn StreamImpl>> {
        Err(Error::NotImplemented("oss"))
    }
}
