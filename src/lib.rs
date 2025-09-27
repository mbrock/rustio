#![feature(core_io_borrowed_buf)]
#![feature(read_buf)]
#![feature(maybe_uninit_slice)]
#![feature(maybe_uninit_write_slice)]

pub mod sinks;
pub mod writer;

pub use sinks::WriteDrain;
pub use writer::{Drain, Sink, WritableStream};
