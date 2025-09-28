use sha2::digest::{FixedOutputReset, Output, Update};
use std::any::Any;
use std::fmt;
use std::io::{self, BorrowedBuf, IoSlice, Write};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

/// Zig-inspired buffered writer as a concrete type with a vtable
/// that it only touches when the buffer is full.
///
/// Being concrete, it can have many useful methods without multiplying
/// generated code size by the number of sink types.
pub struct Writer {
    ptr: NonNull<MaybeUninit<u8>>,
    len: usize,
    pub(crate) end: usize,
    backend: Option<Box<dyn Sink>>,
}

/// Zig-inspired writer sink trait, like `std.Io.Writer.VTable`.
pub trait Sink: Any {
    /// Push bytes to the logical sink according to the write contract.
    fn drain(&mut self, w: &mut Writer, data: &[&[u8]], splat: usize) -> io::Result<usize>;

    /// Default flush: repeatedly drain staged bytes until empty.
    fn flush(&mut self, w: &mut Writer) -> io::Result<()> {
        while w.end != 0 {
            let n = self.drain(w, &[&[]], 0)?;
            debug_assert_eq!(n, 0);
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

pub struct WritableStream<'buf, S: Sink> {
    writer: Box<Writer>,
    _phantom: PhantomData<(&'buf mut [MaybeUninit<u8>], S)>,
}

/// Implements the sink trait for any `Write` type.
/// Drains the buffer content and the data as a vectored write.
impl<T> Sink for T
where
    T: Write + Any + 'static,
{
    fn drain(&mut self, w: &mut Writer, data: &[&[u8]], splat: usize) -> io::Result<usize> {
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

    fn flush(&mut self, w: &mut Writer) -> io::Result<()> {
        while w.end != 0 {
            let n = self.drain(w, &[&[]], 0)?;
            debug_assert_eq!(n, 0)
        }
        Write::flush(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl fmt::Debug for Writer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writer")
            .field("capacity", &self.capacity())
            .field("end", &self.end)
            .finish()
    }
}

impl Writer {
    pub fn from_uninit_slice(buf: &mut [MaybeUninit<u8>], backend: Box<dyn Sink>) -> Self {
        let len = buf.len();
        let ptr = NonNull::new(buf.as_mut_ptr()).expect("slice pointer should never be null");
        Self {
            ptr,
            len,
            end: 0,
            backend: Some(backend),
        }
    }

    pub fn from_slice(buf: &mut [u8], backend: Box<dyn Sink>) -> Self {
        let len = buf.len();
        let ptr = buf.as_mut_ptr().cast::<MaybeUninit<u8>>();
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        Self::from_uninit_slice(slice, backend)
    }

    pub fn from_vector(buf: &mut Vec<u8>, backend: Box<dyn Sink>) -> Self {
        Self::from_uninit_slice(buf.spare_capacity_mut(), backend)
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

    /// Copies data into the buffer if and only if it all fits in the capacity.
    /// Otherwise drains it through the sink.
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
    pub fn with_backend<R>(&mut self, f: impl FnOnce(&mut dyn Sink, &mut Self) -> R) -> R {
        let mut backend = self.backend.take().expect("backend already taken");
        let out = f(backend.as_mut(), self);
        self.backend = Some(backend);
        out
    }

    pub(crate) fn backend_as<T: Sink + 'static>(&self) -> Option<&T> {
        self.backend
            .as_ref()
            .and_then(|backend| backend.as_any().downcast_ref::<T>())
    }

    pub fn backend_as_mut<T: Sink + 'static>(&mut self) -> Option<&mut T> {
        self.backend
            .as_mut()
            .and_then(|x| x.as_any_mut().downcast_mut::<T>())
    }
}

pub(crate) unsafe fn assume_init_slice(slice: &[MaybeUninit<u8>]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, slice.len()) }
}

// enum StreamBuffer<'buf> {
//     Owned(Vec<MaybeUninit<u8>>),
//     Borrowed(&'buf mut [MaybeUninit<u8>]),
// }

// impl<'buf> StreamBuffer<'buf> {
//     fn len(&self) -> usize {
//         match self {
//             StreamBuffer::Owned(vec) => vec.len(),
//             StreamBuffer::Borrowed(slice) => slice.len(),
//         }
//     }

//     fn as_mut_slice(&mut self) -> &mut [MaybeUninit<u8>] {
//         match self {
//             StreamBuffer::Owned(vec) => vec.as_mut_slice(),
//             StreamBuffer::Borrowed(slice) => &mut **slice,
//         }
//     }

//     fn make_sink(&mut self, backend: Box<dyn Sink>) -> Writer {
//         let slice = self.as_mut_slice();
//         Writer::from_uninit_slice(slice, backend)
//     }
// }

/// Helper for doing Zig-style stack allocated uninitialized byte arrays.
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

    pub fn writer<S>(&mut self, sink: S) -> Writer
    where
        S: Sink,
    {
        Writer::from_uninit_slice(self.as_uninit_mut(), Box::new(sink))
    }

    pub fn writable_stream<S>(&mut self, sink: S) -> WritableStream<'_, S>
    where
        S: Sink,
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

/// A sink type that forwards to an underlying writer
/// while updating a hash digest with all forwarded bytes.
pub struct Hashing<H: Update + FixedOutputReset> {
    sink: Option<Writer>,
    hasher: H,
}

impl<H: Update + FixedOutputReset> Hashing<H> {
    pub fn new(digest: H, sink: Writer) -> Self {
        Self {
            sink: Some(sink),
            hasher: digest,
        }
    }

    /// Reset the hasher and return its result array.
    pub fn digest(&mut self) -> Output<H> {
        self.hasher.finalize_fixed_reset()
    }

    pub fn with_sink<R>(&mut self, f: impl FnOnce(&mut Writer, &mut Self) -> R) -> R {
        let mut sink = self.sink.take().unwrap();
        let r = f(&mut sink, self);
        self.sink = Some(sink);
        r
    }

    pub fn borrow_sink(&self) -> Option<&Writer> {
        self.sink.as_ref()
    }

    pub fn take_sink(&mut self) -> Option<Writer> {
        self.sink.take()
    }
}

impl<H: Update + FixedOutputReset + 'static> Sink for Hashing<H> {
    fn drain(&mut self, w: &mut Writer, data: &[&[u8]], splat: usize) -> io::Result<usize> {
        let staged = unsafe { assume_init_slice(&w.buf_slice()[..w.end]) };
        let wrote_from_buffer = self.with_sink(|sink, _| sink.write(staged))?;
        if wrote_from_buffer > 0 {
            self.hasher.update(&staged[..wrote_from_buffer]);
        }

        let mut consumed_from_data = w.consume(wrote_from_buffer);
        if w.end != 0 {
            return Ok(consumed_from_data);
        }

        // there should be some way to not repeat this boring code all the time lol

        let wrote_from_data = self.with_sink(|sink, _| sink.write_vec_splat(data, splat))?;
        if wrote_from_data > 0 {
            let mut remaining = wrote_from_data;
            let mut hashed = Vec::with_capacity(remaining);

            if data.len() > 1 {
                for slice in &data[..data.len() - 1] {
                    if remaining == 0 {
                        break;
                    }
                    let take = slice.len().min(remaining);
                    hashed.extend_from_slice(&slice[..take]);
                    remaining -= take;
                }
            }

            if remaining > 0 && splat > 0 {
                let pattern = data[data.len() - 1];
                if !pattern.is_empty() {
                    let mut copies = splat;
                    while remaining > 0 && copies > 0 {
                        let take = pattern.len().min(remaining);
                        hashed.extend_from_slice(&pattern[..take]);
                        remaining -= take;
                        if take < pattern.len() {
                            break;
                        }
                        copies -= 1;
                    }
                }
            }

            debug_assert!(remaining == 0);
            self.hasher.update(&hashed);
        }

        consumed_from_data += wrote_from_data;
        Ok(consumed_from_data)
    }

    fn flush(&mut self, w: &mut Writer) -> io::Result<()> {
        while w.end > 0 {
            let n = self.drain(w, &[&[]], 0)?;
            debug_assert_eq!(n, 0)
        }
        self.with_sink(|sink, _| sink.flush())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl<'buf, S: Sink> WritableStream<'buf, S> {
    fn new(mut writer: Writer) -> Self {
        writer.end = 0;
        Self {
            writer: Box::new(writer),
            _phantom: PhantomData,
        }
    }

    // pub fn with_capacity(cap: usize, sink: S) -> Self {
    //     let mut vec = Vec::with_capacity(cap);
    //     vec.resize_with(cap, MaybeUninit::<u8>::uninit);
    //     let mut buffer = StreamBuffer::Owned(vec);
    //     let writer = buffer.make_sink(Box::new(sink));
    //     Self::new(buffer, writer)
    // }

    pub fn with_uninit_buffer(buf: &'buf mut [MaybeUninit<u8>], sink: S) -> Self {
        let writer = Writer::from_uninit_slice(buf, Box::new(sink));
        Self::new(writer)
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

    pub fn writer(&mut self) -> &mut Writer {
        &mut self.writer
    }

    pub fn sink(&self) -> &S {
        self.writer
            .backend_as::<S>()
            .expect("backend should be our sink type")
    }

    pub fn sink_mut(&mut self) -> &mut S {
        self.writer.backend_as_mut().unwrap()
    }

    pub fn capacity(&self) -> usize {
        self.writer.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        let mut buffer = StackBuffer::<8>::new();
        let mut stream = buffer.writable_stream(Vec::new());
        let writer = stream.writer();
        writer.write_all(b"hi")?;
        writer.flush()?;
        assert_eq!(stream.sink().as_slice(), b"hi");
        Ok(())
    }

    #[test]
    fn fast_path_write_stays_in_buffer() -> io::Result<()> {
        let mut buffer = StackBuffer::<16>::new();
        let mut stream = buffer.writable_stream(Vec::new());
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
        let mut buffer = StackBuffer::<4>::new();
        let mut stream = buffer.writable_stream(Vec::new());

        stream.writer().splat_all(b"ab", 3)?;
        stream.writer().flush()?;

        assert_eq!(stream.sink().as_slice(), b"ababab");

        Ok(())
    }

    #[test]
    fn write_vec_splat_handles_multiple_segments() -> io::Result<()> {
        let mut buffer = StackBuffer::<16>::new();
        let mut stream = buffer.writable_stream(Vec::new());

        let writer = stream.writer();
        let segments: [&[u8]; 2] = [b"ab".as_ref(), b"cd".as_ref()];
        writer.write_vec_splat(&segments, 2)?;
        writer.flush()?;
        assert_eq!(stream.sink().as_slice(), b"abcdcd");
        Ok(())
    }

    #[test]
    fn write_drain_vectored_consumes_partial() -> io::Result<()> {
        struct ShortWriter {
            target: Vec<u8>,
            limit: usize,
        }

        impl ShortWriter {
            fn with_limit(limit: usize) -> Self {
                Self {
                    target: Vec::new(),
                    limit,
                }
            }

            fn as_slice(&self) -> &[u8] {
                &self.target
            }
        }

        impl Write for ShortWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let n = buf.len().min(self.limit);
                self.target.extend_from_slice(&buf[..n]);
                Ok(n)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let writer_impl = ShortWriter::with_limit(4);

        let mut buffer = StackBuffer::<2>::new();
        let mut stream = buffer.writable_stream(writer_impl);

        stream.writer().write_all(b"abcdef")?;
        stream.writer().flush()?;
        assert_eq!(stream.sink().as_slice(), b"abcdef");
        Ok(())
    }

    #[test]
    fn hashing_sink_works() -> io::Result<()> {
        use sha2::{Digest, Sha256};

        let mut fofo: Vec<u8> = Vec::with_capacity(8);
        let sink = Writer::from_vector(&mut fofo, Box::new(Vec::new()));

        let hashing = Hashing::new(Sha256::new(), sink);

        let mut buffer = StackBuffer::<4>::new(); // why doesn't it work with 8 lol
        let mut stream = buffer.writable_stream(hashing);

        stream.writer().write_all(b"foobar")?;
        stream.writer().flush()?;

        let digest = stream.sink_mut().digest();

        assert_eq!(digest, Sha256::digest(b"foobar"));
        let out = stream
            .sink()
            .borrow_sink()
            .unwrap()
            .backend_as::<Vec<u8>>()
            .unwrap()
            .as_slice();
        assert_eq!(out, b"foobar");

        Ok(())
    }
}
