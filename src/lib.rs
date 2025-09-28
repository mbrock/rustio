#![feature(core_io_borrowed_buf)]
#![feature(read_buf)]
#![feature(maybe_uninit_slice)]
#![feature(maybe_uninit_write_slice)]

pub mod writer;

pub use writer::{Hashing, Sink, StackBuffer, WritableStream, Writer};
