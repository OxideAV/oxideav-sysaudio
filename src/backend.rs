//! Internal backend trait. Each module under `backends/` implements
//! this; the public API iterates over a static slice of `&dyn Backend`.

use crate::format::{CallbackInfo, StreamRequest};
use crate::stream::StreamImpl;
use crate::Result;

pub(crate) type Callback = Box<dyn FnMut(&mut [f32], &CallbackInfo) + Send + 'static>;

pub(crate) trait Backend: Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// Cheap check that this backend is usable right now: loads the
    /// shared library and opens a throw-away handle. Errors are
    /// swallowed by `probe()`; they don't bubble to the user.
    fn probe(&self) -> Result<()>;

    /// Open a real output stream. Returns a boxed [`StreamImpl`] the
    /// caller wraps in [`crate::Stream`].
    fn open(&self, req: StreamRequest, cb: Callback) -> Result<Box<dyn StreamImpl>>;
}
