use std::any::Any;
use std::fmt;
use std::io::{self, BorrowedBuf};
use std::marker::PhantomData;
use std::mem::MaybeUninit;

/// Internal buffer abstraction allowing either owned or borrowed backing storage.
enum Buffer<'a> {
    Owned(Vec<MaybeUninit<u8>>),
    Borrowed {
        ptr: *mut MaybeUninit<u8>,
        len: usize,
        _marker: PhantomData<&'a mut [MaybeUninit<u8>]>,
    },
}

impl<'a> Buffer<'a> {
    fn len(&self) -> usize {
        match self {
            Buffer::Owned(vec) => vec.len(),
            Buffer::Borrowed { len, .. } => *len,
        }
    }

    fn as_slice(&self) -> &[MaybeUninit<u8>] {
        match self {
            Buffer::Owned(vec) => vec.as_slice(),
            Buffer::Borrowed { ptr, len, .. } => unsafe {
                std::slice::from_raw_parts(*ptr as *const MaybeUninit<u8>, *len)
            },
        }
    }

    fn as_mut_slice(&mut self) -> &mut [MaybeUninit<u8>] {
        match self {
            Buffer::Owned(vec) => vec.as_mut_slice(),
            Buffer::Borrowed { ptr, len, .. } => unsafe {
                std::slice::from_raw_parts_mut(*ptr, *len)
            },
        }
    }
}

/// Primary sink trait mirroring the Zig `drain` contract.
pub trait Drain: Any {
    /// Push bytes to the logical sink according to the write contract.
    fn drain(&mut self, w: &mut Sink<'_>, data: &[&[u8]], splat: usize) -> io::Result<usize>;

    /// Default flush: repeatedly drain staged bytes until empty.
    fn flush(&mut self, w: &mut Sink<'_>) -> io::Result<()> {
        while w.end != 0 {
            let n = self.drain(w, &[&[]], 0)?;
            debug_assert_eq!(n, 0);
        }
        Ok(())
    }

    /// Default rebase: drain-old + slide preserved tail until capacity available.
    /// Keeps things simple: assumes the request is feasible for fixed-capacity buffers.
    #[allow(dead_code)]
    fn rebase(&mut self, w: &mut Sink<'_>, preserve: usize, need: usize) -> io::Result<()> {
        while w.unused_len() < need {
            let preserved_head = w.end.saturating_sub(preserve);
            let preserved_len = w.end - preserved_head;
            w.end = preserved_head;

            // Ask sink to flush older bytes (may be partial).
            let n = self.drain(w, &[&[]], 0)?;
            debug_assert_eq!(n, 0);

            // Slide the preserved tail down.
            let dst = w.end;
            {
                let buf = w.buf_slice_mut();
                buf.copy_within(preserved_head..preserved_head + preserved_len, dst);
            }
            w.end += preserved_len;

            assert!(
                w.capacity() - preserve >= need,
                "rebase request exceeds fixed capacity; a growing writer should override rebase"
            );
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any;
}

/// A concrete writer: owns or borrows an uninitialized buffer + staged cursor,
/// and routes operations to the Drain backend.
pub struct Sink<'a> {
    buf: Buffer<'a>,
    pub(crate) end: usize,
    backend: Option<Box<dyn Drain>>,
}

impl fmt::Debug for Sink<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writer")
            .field("capacity", &self.capacity())
            .field("end", &self.end)
            .finish()
    }
}

impl<'a> Sink<'a> {
    pub fn with_capacity(cap: usize, backend: Box<dyn Drain>) -> Self {
        let mut buf = Vec::with_capacity(cap);
        buf.resize_with(cap, MaybeUninit::<u8>::uninit);
        Self {
            buf: Buffer::Owned(buf),
            end: 0,
            backend: Some(backend),
        }
    }

    pub fn from_uninit_slice(buf: &'a mut [MaybeUninit<u8>], backend: Box<dyn Drain>) -> Self {
        Self {
            buf: Buffer::Borrowed {
                ptr: buf.as_mut_ptr(),
                len: buf.len(),
                _marker: PhantomData,
            },
            end: 0,
            backend: Some(backend),
        }
    }

    pub fn from_slice(buf: &'a mut [u8], backend: Box<dyn Drain>) -> Self {
        let len = buf.len();
        let ptr = buf.as_mut_ptr().cast::<MaybeUninit<u8>>();
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        Self::from_uninit_slice(slice, backend)
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub fn unused_len(&self) -> usize {
        self.capacity() - self.end
    }

    pub(crate) fn buf_slice(&self) -> &[MaybeUninit<u8>] {
        self.buf.as_slice()
    }

    pub(crate) fn buf_slice_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.buf.as_mut_slice()
    }

    /// Returns the staged prefix as &[u8].
    pub fn with_filled<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        let filled = unsafe { assume_init_slice(&self.buf_slice()[..self.end]) };
        f(filled)
    }

    /// Returns the unfilled tail as a BorrowedBuf; updates `end` on return.
    pub fn with_unfilled<R>(&mut self, f: impl FnOnce(&mut BorrowedBuf<'_>) -> R) -> R {
        let len = self.capacity();
        let end = self.end;
        let tail: &mut [MaybeUninit<u8>] = {
            let slice = self.buf_slice_mut();
            &mut slice[end..len]
        };
        let mut bb = BorrowedBuf::from(tail);
        let res = f(&mut bb);
        self.end += bb.len();
        res
    }

    /// Core write primitive mirroring Zig's `writev` + `splat` contract.
    ///
    /// - `data` must be non-empty.
    /// - All slices except the last are written once in order.
    /// - The final slice is written `splat` times.
    ///
    /// Returns the number of bytes consumed from `data` (excluding staged bytes).
    pub fn write_vec_splat(&mut self, data: &[&[u8]], splat: usize) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }

        let mut required = 0usize;
        if data.len() > 1 {
            required += data[..data.len() - 1]
                .iter()
                .map(|s| s.len())
                .sum::<usize>();
        }
        let last = data[data.len() - 1];
        required = required.saturating_add(last.len().saturating_mul(splat));

        if required == 0 {
            return Ok(0);
        }

        if self.unused_len() >= required {
            let mut cursor = self.end;
            {
                let buf = self.buf_slice_mut();
                if data.len() > 1 {
                    for slice in &data[..data.len() - 1] {
                        let len = slice.len();
                        if len == 0 {
                            continue;
                        }
                        buf[cursor..cursor + len].write_copy_of_slice(slice);
                        cursor += len;
                    }
                }
                if splat > 0 {
                    let len = last.len();
                    if len > 0 {
                        for _ in 0..splat {
                            buf[cursor..cursor + len].write_copy_of_slice(last);
                            cursor += len;
                        }
                    }
                }
            }
            self.end = cursor;
            Ok(required)
        } else {
            self.with_backend(|backend, me| backend.drain(me, data, splat))
        }
    }

    /// Fast path memcpy into tail; else call vtable drain.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_vec_splat(&[bytes], 1)
    }

    pub fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = self.write_vec_splat(&[bytes], 1)?;
            if n == 0 {
                continue;
            }
            bytes = &bytes[n..];
        }
        Ok(())
    }

    pub fn splat_all(&mut self, unit: &[u8], count: usize) -> io::Result<()> {
        if count == 0 {
            return Ok(());
        }
        self.write_vec_splat(&[unit], count)?;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.with_backend(|backend, me| backend.flush(me))
    }

    #[allow(dead_code)]
    pub fn rebase(&mut self, preserve: usize, need: usize) -> io::Result<()> {
        if self.unused_len() >= need {
            return Ok(());
        }
        self.with_backend(|backend, me| backend.rebase(me, preserve, need))
    }

    /// Slide out `n` bytes from the staged prefix.
    /// Returns how many bytes that implies *beyond* the buffer (i.e., from `data`).
    pub(crate) fn consume(&mut self, n: usize) -> usize {
        let end = self.end;
        if n < end {
            let remain = end - n;
            {
                let buf = self.buf_slice_mut();
                buf.copy_within(n..end, 0);
            }
            self.end = remain;
            0
        } else {
            let overflow = n - self.end;
            self.end = 0;
            overflow
        }
    }

    #[inline]
    fn with_backend<R>(&mut self, f: impl FnOnce(&mut dyn Drain, &mut Self) -> R) -> R {
        let mut backend = self.backend.take().expect("backend already taken");
        let out = f(backend.as_mut(), self);
        self.backend = Some(backend);
        out
    }

    pub(crate) fn backend_as<T: Drain + 'static>(&self) -> Option<&T> {
        self.backend
            .as_ref()
            .and_then(|backend| backend.as_any().downcast_ref::<T>())
    }
}

pub(crate) unsafe fn assume_init_slice(slice: &[MaybeUninit<u8>]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, slice.len()) }
}

/// Generic Zig-style adapter that owns a writer configured with a specific sink.
pub struct WritableStream<'buf, S: Drain + 'static> {
    writer: Sink<'buf>,
    _marker: PhantomData<S>,
}

impl<'buf, S: Drain + 'static> WritableStream<'buf, S> {
    fn from_sink(writer: Sink<'buf>) -> Self {
        Self {
            writer,
            _marker: PhantomData,
        }
    }

    pub fn with_capacity(cap: usize, sink: S) -> Self {
        Self::from_sink(Sink::with_capacity(cap, Box::new(sink)))
    }

    pub fn with_uninit_buffer(buf: &'buf mut [MaybeUninit<u8>], sink: S) -> Self {
        Self::from_sink(Sink::from_uninit_slice(buf, Box::new(sink)))
    }

    pub fn with_stack(buf: &'buf mut [u8], sink: S) -> Self {
        Self::from_sink(Sink::from_slice(buf, Box::new(sink)))
    }

    pub fn writer(&mut self) -> &mut Sink<'buf> {
        &mut self.writer
    }

    pub fn sink(&self) -> &S {
        self.writer
            .backend_as::<S>()
            .expect("backend should be our sink type")
    }
}

#[cfg(test)]
mod tests {
    use crate::WriteDrain;

    use super::*;

    #[test]
    fn stack_buffer_to_allocating_drain() -> io::Result<()> {
        let mut buf = [0u8; 16];
        let mut stream = WritableStream::with_stack(&mut buf, WriteDrain::new(Vec::new()));
        stream.writer().write_all(b"stack")?;
        stream.writer().flush()?;
        assert_eq!(stream.sink().out.as_ref(), b"stack");
        Ok(())
    }

    #[test]
    fn owned_buffer_to_allocating_drain() -> io::Result<()> {
        let mut stream = WritableStream::with_capacity(8, WriteDrain::new(Vec::new()));
        let writer = stream.writer();
        writer.write_all(b"hi")?;
        writer.flush()?;
        assert_eq!(stream.sink().out.as_ref(), b"hi");
        Ok(())
    }

    #[test]
    fn fast_path_write_stays_in_buffer() -> io::Result<()> {
        let mut stream = WritableStream::with_capacity(16, WriteDrain::new(Vec::new()));
        let writer = stream.writer();
        writer.write_all(b"hi")?;
        writer.with_filled(|filled| assert_eq!(filled, b"hi"));
        assert_eq!(stream.sink().out.as_ref(), b"");
        stream.writer().flush()?;
        assert_eq!(stream.sink().out.as_ref(), b"hi");
        Ok(())
    }

    #[test]
    fn splat_all_repeats_pattern_when_overflowing() -> io::Result<()> {
        let mut stream = WritableStream::with_capacity(4, WriteDrain::new(Vec::new()));

        stream.writer().splat_all(b"ab", 3)?;
        stream.writer().flush()?;

        assert_eq!(stream.sink().out.as_ref(), b"ababab");

        Ok(())
    }

    #[test]
    fn write_vec_splat_handles_multiple_segments() -> io::Result<()> {
        let mut stream = WritableStream::with_capacity(16, WriteDrain::new(Vec::new()));
        let writer = stream.writer();
        let segments: [&[u8]; 2] = [b"ab".as_ref(), b"cd".as_ref()];
        writer.write_vec_splat(&segments, 2)?;
        writer.flush()?;
        assert_eq!(stream.sink().out.as_ref(), b"abcdcd");
        Ok(())
    }
}
