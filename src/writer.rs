use std::any::Any;
use std::fmt;
use std::io::{self, BorrowedBuf, IoSlice, Write};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

/// Primary sink trait mirroring the Zig `drain` contract.
pub trait Drain: Any {
    /// Push bytes to the logical sink according to the write contract.
    fn drain(&mut self, w: &mut Sink, data: &[&[u8]], splat: usize) -> io::Result<usize>;

    /// Default flush: repeatedly drain staged bytes until empty.
    fn flush(&mut self, w: &mut Sink) -> io::Result<()> {
        while w.end != 0 {
            let n = self.drain(w, &[&[]], 0)?;
            debug_assert_eq!(n, 0);
        }
        Ok(())
    }

    /// Default rebase: drain-old + slide preserved tail until capacity available.
    /// Keeps things simple: assumes the request is feasible for fixed-capacity buffers.
    #[allow(dead_code)]
    fn rebase(&mut self, w: &mut Sink, preserve: usize, need: usize) -> io::Result<()> {
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

impl<T> Drain for T
where
    T: Write + Any + 'static,
{
    fn drain(&mut self, w: &mut Sink, data: &[&[u8]], splat: usize) -> io::Result<usize> {
        debug_assert!(!data.is_empty());

        let staged = unsafe { assume_init_slice(&w.buf_slice()[..w.end]) };

        let mut iov = Vec::<IoSlice<'_>>::with_capacity(1 + data.len().saturating_sub(1) + splat);
        if !staged.is_empty() {
            iov.push(IoSlice::new(staged));
        }
        if data.len() > 1 {
            for &segment in &data[..data.len() - 1] {
                if !segment.is_empty() {
                    iov.push(IoSlice::new(segment));
                }
            }
        }
        let pattern = data[data.len() - 1];
        for _ in 0..splat {
            iov.push(IoSlice::new(pattern));
        }

        if iov.is_empty() {
            return Ok(0);
        }

        let wrote = self.write_vectored(&iov)?;
        if wrote == 0 && (!staged.is_empty() || splat > 0 || data.iter().any(|s| !s.is_empty())) {
            return Ok(0);
        }

        let from_data = w.consume(wrote);
        Ok(from_data)
    }

    fn flush(&mut self, w: &mut Sink) -> io::Result<()> {
        while w.end != 0 {
            let n = self.drain(w, &[&[]], 0)?;
            if n == 0 && w.end != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "drain made no progress",
                ));
            }
        }
        Write::flush(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A concrete writer: owns or borrows an uninitialized buffer + staged cursor,
/// and routes operations to the Drain backend.
pub struct Sink {
    ptr: NonNull<MaybeUninit<u8>>,
    len: usize,
    pub(crate) end: usize,
    backend: Option<Box<dyn Drain>>,
}

impl fmt::Debug for Sink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writer")
            .field("capacity", &self.capacity())
            .field("end", &self.end)
            .finish()
    }
}

impl Sink {
    pub fn from_uninit_slice(buf: &mut [MaybeUninit<u8>], backend: Box<dyn Drain>) -> Self {
        let len = buf.len();
        let ptr = NonNull::new(buf.as_mut_ptr()).expect("slice pointer should never be null");
        Self {
            ptr,
            len,
            end: 0,
            backend: Some(backend),
        }
    }

    pub fn from_slice(buf: &mut [u8], backend: Box<dyn Drain>) -> Self {
        let len = buf.len();
        let ptr = buf.as_mut_ptr().cast::<MaybeUninit<u8>>();
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        Self::from_uninit_slice(slice, backend)
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn unused_len(&self) -> usize {
        self.capacity() - self.end
    }

    pub(crate) fn buf_slice(&self) -> &[MaybeUninit<u8>] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub(crate) fn buf_slice_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
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

enum StreamBuffer<'buf> {
    Owned(Vec<MaybeUninit<u8>>),
    Borrowed(&'buf mut [MaybeUninit<u8>]),
}

impl<'buf> StreamBuffer<'buf> {
    fn len(&self) -> usize {
        match self {
            StreamBuffer::Owned(vec) => vec.len(),
            StreamBuffer::Borrowed(slice) => slice.len(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [MaybeUninit<u8>] {
        match self {
            StreamBuffer::Owned(vec) => vec.as_mut_slice(),
            StreamBuffer::Borrowed(slice) => &mut **slice,
        }
    }

    fn make_sink(&mut self, backend: Box<dyn Drain>) -> Sink {
        let slice = self.as_mut_slice();
        Sink::from_uninit_slice(slice, backend)
    }
}

/// Stack-allocated byte storage for `WritableStream`, wrapping a `[MaybeUninit<u8>; N]`.
pub struct StackBuffer<const N: usize> {
    data: [MaybeUninit<u8>; N],
}

impl<const N: usize> StackBuffer<N> {
    pub const fn new() -> Self {
        Self {
            data: [MaybeUninit::uninit(); N],
        }
    }

    pub const fn capacity(&self) -> usize {
        N
    }

    pub fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        &mut self.data
    }

    pub fn filled(&self, len: usize) -> &[u8] {
        assert!(len <= N, "filled length exceeds capacity");
        unsafe { assume_init_slice(&self.data[..len]) }
    }

    pub fn writable_stream<S>(&mut self, sink: S) -> WritableStream<'_, S>
    where
        S: Drain + 'static,
    {
        WritableStream::with_buffer(self, sink)
    }
}

impl<const N: usize> Default for StackBuffer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> AsMut<[MaybeUninit<u8>]> for StackBuffer<N> {
    fn as_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.as_uninit_mut()
    }
}

/// Generic Zig-style adapter that owns a writer configured with a specific sink.
pub struct WritableStream<'buf, S: Drain + 'static> {
    buffer: StreamBuffer<'buf>,
    writer: Sink,
    _marker: PhantomData<S>,
}

impl<'buf, S: Drain + 'static> WritableStream<'buf, S> {
    fn new(buffer: StreamBuffer<'buf>, mut writer: Sink) -> Self {
        debug_assert_eq!(buffer.len(), writer.capacity());
        writer.end = 0;
        Self {
            buffer,
            writer,
            _marker: PhantomData,
        }
    }

    pub fn with_capacity(cap: usize, sink: S) -> Self {
        let mut vec = Vec::with_capacity(cap);
        vec.resize_with(cap, MaybeUninit::<u8>::uninit);
        let mut buffer = StreamBuffer::Owned(vec);
        let writer = buffer.make_sink(Box::new(sink));
        Self::new(buffer, writer)
    }

    pub fn with_uninit_buffer(buf: &'buf mut [MaybeUninit<u8>], sink: S) -> Self {
        let mut buffer = StreamBuffer::Borrowed(buf);
        let writer = buffer.make_sink(Box::new(sink));
        Self::new(buffer, writer)
    }

    pub fn with_slice(buf: &'buf mut [u8], sink: S) -> Self {
        let len = buf.len();
        let ptr = buf.as_mut_ptr().cast::<MaybeUninit<u8>>();
        let uninit = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        Self::with_uninit_buffer(uninit, sink)
    }

    pub fn with_buffer<B>(buffer: &'buf mut B, sink: S) -> Self
    where
        B: AsMut<[MaybeUninit<u8>]>,
    {
        Self::with_uninit_buffer(buffer.as_mut(), sink)
    }

    pub fn writer(&mut self) -> &mut Sink {
        &mut self.writer
    }

    pub fn sink(&self) -> &S {
        self.writer
            .backend_as::<S>()
            .expect("backend should be our sink type")
    }

    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{IoSlice, Write};
    use std::rc::Rc;

    #[test]
    fn stack_buffer_to_allocating_drain() -> io::Result<()> {
        let mut storage = StackBuffer::<16>::new();
        let mut stream = storage.writable_stream(Vec::new());

        stream.writer().write_all(b"stack")?;
        stream.writer().flush()?;
        assert_eq!(stream.sink().as_slice(), b"stack");
        Ok(())
    }

    #[test]
    fn stack_buffer_reuse_across_streams() -> io::Result<()> {
        let mut storage = StackBuffer::<16>::new();

        {
            let mut stream = storage.writable_stream(Vec::new());
            stream.writer().write_all(b"first")?;
            stream.writer().flush()?;
            let collected = stream.sink().as_slice().to_vec();
            assert_eq!(collected.as_slice(), b"first");
            let len = collected.len();
            drop(stream);
            assert_eq!(storage.filled(len), collected.as_slice());
        }

        {
            let mut stream = storage.writable_stream(Vec::new());
            stream.writer().write_all(b"second")?;
            stream.writer().flush()?;
            let collected = stream.sink().as_slice().to_vec();
            assert_eq!(collected.as_slice(), b"second");
            let len = collected.len();
            drop(stream);
            assert_eq!(storage.filled(len), collected.as_slice());
        }

        Ok(())
    }

    #[test]
    fn owned_buffer_to_allocating_drain() -> io::Result<()> {
        let mut stream = WritableStream::with_capacity(8, Vec::new());
        let writer = stream.writer();
        writer.write_all(b"hi")?;
        writer.flush()?;
        assert_eq!(stream.sink().as_slice(), b"hi");
        Ok(())
    }

    #[test]
    fn fast_path_write_stays_in_buffer() -> io::Result<()> {
        let mut stream = WritableStream::with_capacity(16, Vec::new());
        let writer = stream.writer();
        writer.write_all(b"hi")?;
        writer.with_filled(|filled| assert_eq!(filled, b"hi"));
        assert_eq!(stream.sink().as_slice(), b"");
        stream.writer().flush()?;
        assert_eq!(stream.sink().as_slice(), b"hi");
        Ok(())
    }

    #[test]
    fn splat_all_repeats_pattern_when_overflowing() -> io::Result<()> {
        let mut stream = WritableStream::with_capacity(4, Vec::new());

        stream.writer().splat_all(b"ab", 3)?;
        stream.writer().flush()?;

        assert_eq!(stream.sink().as_slice(), b"ababab");

        Ok(())
    }

    #[test]
    fn write_vec_splat_handles_multiple_segments() -> io::Result<()> {
        let mut stream = WritableStream::with_capacity(16, Vec::new());
        let writer = stream.writer();
        let segments: [&[u8]; 2] = [b"ab".as_ref(), b"cd".as_ref()];
        writer.write_vec_splat(&segments, 2)?;
        writer.flush()?;
        assert_eq!(stream.sink().as_slice(), b"abcdcd");
        Ok(())
    }

    #[test]
    fn write_drain_vectored_consumes_partial() -> io::Result<()> {
        #[derive(Clone)]
        struct ShortWriter {
            target: Rc<RefCell<Vec<u8>>>,
            limit: usize,
        }

        impl Write for ShortWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let n = buf.len().min(self.limit);
                self.target.borrow_mut().extend_from_slice(&buf[..n]);
                Ok(n)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }

            fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
                let mut remaining = self.limit;
                let mut wrote = 0;
                for slice in bufs {
                    if remaining == 0 {
                        break;
                    }
                    let n = slice.len().min(remaining);
                    if n > 0 {
                        self.target.borrow_mut().extend_from_slice(&slice[..n]);
                        remaining -= n;
                        wrote += n;
                    }
                }
                Ok(wrote)
            }
        }

        let target = Rc::new(RefCell::new(Vec::new()));
        let writer_impl = ShortWriter {
            target: target.clone(),
            limit: 4,
        };
        let mut stream = WritableStream::with_capacity(2, writer_impl);
        stream.writer().write_all(b"abcdef")?;
        stream.writer().flush()?;
        assert_eq!(target.borrow().as_slice(), b"abcdef");
        Ok(())
    }
}
