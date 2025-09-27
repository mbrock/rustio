#![feature(core_io_borrowed_buf)]
#![feature(read_buf)]
#![feature(maybe_uninit_slice)]
#![feature(maybe_uninit_write_slice)]

use std::any::Any;
use std::fmt;
use std::io::{self, BorrowedBuf, Cursor, IoSlice, Read};
use std::marker::PhantomData;
use std::mem::MaybeUninit;

/// Backing storage for a writer, owned or user-provided.
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

/// The single primitive sink interface (vtable).
trait Drain: Any {
    /// Push bytes to the logical sink according to the write contract.
    fn drain(&mut self, w: &mut Writer<'_>, data: &[&[u8]], splat: usize) -> io::Result<usize>;

    /// Default flush: repeatedly drain the staged bytes until empty.
    fn flush(&mut self, w: &mut Writer<'_>) -> io::Result<()> {
        while w.end != 0 {
            let n = self.drain(w, &[b""], 1)?;
            debug_assert_eq!(n, 0);
        }
        Ok(())
    }

    /// Default rebase: drain-old + slide preserved tail until capacity available.
    /// Keeps things simple: assumes the request is feasible for fixed-capacity buffers.
    fn rebase(&mut self, w: &mut Writer<'_>, preserve: usize, need: usize) -> io::Result<()> {
        while w.unused_len() < need {
            let preserved_head = w.end.saturating_sub(preserve);
            let preserved_len = w.end - preserved_head;
            w.end = preserved_head;

            // Ask sink to flush older bytes (may be partial).
            let n = self.drain(w, &[b""], 1)?;
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
                "Demo rebase: request exceeds fixed capacity; \
                 a growing writer should override rebase"
            );
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any;
}

/// A simple sink that collects everything into an owned Vec<u8>,
/// using vectored writes for efficiency.
struct MemorySink {
    out: Vec<u8>,
}

impl MemorySink {
    fn new() -> Self {
        Self { out: Vec::new() }
    }
}

impl Drain for MemorySink {
    fn drain(&mut self, w: &mut Writer<'_>, data: &[&[u8]], splat: usize) -> io::Result<usize> {
        // Build an IoSlice list: [staged, data[0], data[1], ..., pattern x K]
        let staged = unsafe { assume_init_slice(&w.buf_slice()[..w.end]) };

        let mut iov = Vec::<IoSlice<'_>>::with_capacity(1 + data.len() + 8);
        if !staged.is_empty() {
            iov.push(IoSlice::new(staged));
        }
        let pattern = if data.is_empty() { &b""[..] } else { data[data.len() - 1] };
        for &s in &data[..data.len().saturating_sub(1)] {
            if !s.is_empty() {
                iov.push(IoSlice::new(s));
            }
        }
        // Repeat the pattern a bounded number of times this call (avoid huge iovecs).
        if !pattern.is_empty() && splat > 0 {
            let reps = splat.min(8);
            for _ in 0..reps {
                iov.push(IoSlice::new(pattern));
            }
        }

        // "Writev" into our Vec<u8> (no syscalls here, but mirrors the shape).
        let wrote = iov.iter().map(|s| s.len()).sum::<usize>();
        for s in &iov {
            self.out.extend_from_slice(s);
        }

        // Update staged prefix; compute "from data" portion to return.
        let from_data = w.consume(wrote);
        Ok(from_data)
    }

    fn as_any(&self) -> &dyn Any { self }
}

/// A concrete writer: owns or borrows an uninitialized buffer + staged cursor,
/// and routes operations to the Drain backend.
struct Writer<'a> {
    buf: Buffer<'a>,
    end: usize, // number of *initialized + staged* bytes (prefix)
    backend: Option<Box<dyn Drain>>,
}

impl fmt::Debug for Writer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writer")
            .field("capacity", &self.capacity())
            .field("end", &self.end)
            .finish()
    }
}

impl<'a> Writer<'a> {
    fn with_capacity(cap: usize, backend: Box<dyn Drain>) -> Self {
        let mut buf = Vec::with_capacity(cap);
        // Pre-allocate uninitialized memory (Zig-style "undefined" buffer).
        buf.resize_with(cap, MaybeUninit::<u8>::uninit);
        Self { buf: Buffer::Owned(buf), end: 0, backend: Some(backend) }
    }

    fn from_uninit_slice(buf: &'a mut [MaybeUninit<u8>], backend: Box<dyn Drain>) -> Self {
        Self {
            buf: Buffer::Borrowed { ptr: buf.as_mut_ptr(), len: buf.len(), _marker: PhantomData },
            end: 0,
            backend: Some(backend),
        }
    }

    fn from_slice(buf: &'a mut [u8], backend: Box<dyn Drain>) -> Self {
        let len = buf.len();
        let ptr = buf.as_mut_ptr().cast::<MaybeUninit<u8>>();
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        Self::from_uninit_slice(slice, backend)
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    fn unused_len(&self) -> usize {
        self.capacity() - self.end
    }

    fn buf_slice(&self) -> &[MaybeUninit<u8>] {
        self.buf.as_slice()
    }

    fn buf_slice_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.buf.as_mut_slice()
    }

    #[inline]
    fn with_backend<R>(&mut self, f: impl FnOnce(&mut dyn Drain, &mut Self) -> R) -> R {
        let mut backend = self.backend.take().expect("backend already taken");
        let out = f(backend.as_mut(), self);
        self.backend = Some(backend);
        out
    }

    /// Returns the staged prefix as &[u8].
    fn with_filled<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        let filled = unsafe { assume_init_slice(&self.buf_slice()[..self.end]) };
        f(filled)
    }

    /// Returns the unfilled tail as a BorrowedBuf; updates `end` on return.
    fn with_unfilled<R>(&mut self, f: impl FnOnce(&mut BorrowedBuf<'_>) -> R) -> R {
        let (len, end) = (self.capacity(), self.end);
        let tail: &mut [MaybeUninit<u8>] = {
            let slice = self.buf_slice_mut();
            &mut slice[end..len]
        };
        let mut bb = BorrowedBuf::from(tail);
        let res = f(&mut bb);
        self.end += bb.len();
        res
    }

    /// Fast path memcpy into tail; else call vtable drain.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let start = self.end;
        let len = bytes.len();
        let end = start + len;
        if end <= self.capacity() {
            {
                let slice = self.buf_slice_mut();
                slice[start..end].write_copy_of_slice(bytes);
            }
            self.end = end;
            Ok(len)
        } else {
            self.with_backend(|backend, me| backend.drain(me, &[bytes], 1))
        }
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = self.write(bytes)?;
            if n == 0 { continue; } // "temporary back-pressure" signal
            bytes = &bytes[n..];
        }
        Ok(())
    }

    fn splat_all(&mut self, unit: &[u8], count: usize) -> io::Result<()> {
        let unit_len = unit.len();
        let total = unit_len * count;
        if self.end + total <= self.capacity() {
            let mut cursor = self.end;
            for _ in 0..count {
                {
                    let slice = self.buf_slice_mut();
                    slice[cursor..cursor + unit_len].write_copy_of_slice(unit);
                }
                cursor += unit_len;
            }
            self.end += total;
            Ok(())
        } else {
            // Defer to drain with 'splat'
            self.with_backend(|backend, me| {
                backend.drain(me, &[unit], count)?;
                Ok(())
            })
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.with_backend(|backend, me| backend.flush(me))
    }

    fn rebase(&mut self, preserve: usize, need: usize) -> io::Result<()> {
        if self.unused_len() >= need { return Ok(()); }
        self.with_backend(|backend, me| backend.rebase(me, preserve, need))
    }

    /// Slide out `n` bytes from the staged prefix.
    /// Returns how many bytes that implies *beyond* the buffer (i.e., from `data`).
    fn consume(&mut self, n: usize) -> usize {
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
}

fn main() -> io::Result<()> {
    // Demo backend that accumulates everything we "writev" into it.
    let sink = Box::new(MemorySink::new());
    let mut w = Writer::with_capacity(4096, sink);

    // 1) Stage some bytes via fast path
    w.write_all(b"hello ")?;
    w.write_all(b"world")?;

    // Inspect staged prefix (Zig: w.buffer[0..w.end])
    w.with_filled(|bytes| {
        println!("staged before flush: {:?}", std::str::from_utf8(bytes).unwrap());
    });

    // 2) Flush staged bytes to the sink (via vtable drain/flush)
    w.flush()?;

    println!("after first flush -> sink: {}", access_sink(&w)?);

    // 3) Read directly into the uninitialized tail using BorrowedBuf
    let mut cursor = Cursor::new(b" + from reader");
    let n_read = w.with_unfilled(|buf| {
        cursor.read_buf(buf.unfilled())?;
        Ok::<_, io::Error>(buf.len())
    })?;
    println!("read into tail: {n_read} bytes");

    // 4) Add a few exclamation marks via splat and flush again
    w.splat_all(b"!", 3)?;
    w.flush()?;

    println!("after second flush -> sink: {}", access_sink(&w)?);

    // Demonstrate providing a stack-allocated buffer that can be reused.
    let mut stack_backing = [MaybeUninit::<u8>::uninit(); 64];
    {
        let sink = Box::new(MemorySink::new());
        let mut stack_writer = Writer::from_uninit_slice(&mut stack_backing, sink);
        stack_writer.write_all(b"stack first run")?;
        stack_writer.flush()?;
        println!("stack writer -> {}", access_sink(&stack_writer)?);
    }

    {
        let sink = Box::new(MemorySink::new());
        let mut stack_writer = Writer::from_uninit_slice(&mut stack_backing, sink);
        stack_writer.write_all(b"stack reuse")?;
        stack_writer.flush()?;
        println!("stack writer reuse -> {}", access_sink(&stack_writer)?);
    }

    Ok(())
}

/* ------- Helpers to peek at the MemorySink for the demo ------- */

fn access_sink(w: &Writer<'_>) -> io::Result<String> {
    match w
        .backend
        .as_ref()
        .and_then(|b| b.as_any().downcast_ref::<MemorySink>())
    {
        Some(ms) => Ok(String::from_utf8_lossy(&ms.out).into_owned()),
        None => Err(io::Error::new(io::ErrorKind::Other, "backend is not MemorySink")),
    }
}

unsafe fn assume_init_slice(slice: &[MaybeUninit<u8>]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, slice.len()) }
}
