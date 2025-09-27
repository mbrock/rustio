use std::io::{self, IoSlice, Write};

use crate::writer::{Drain, Sink, assume_init_slice};

// impl Drain for AllocatingDrain {
//     fn drain(&mut self, w: &mut Sink<'_>, data: &[&[u8]], splat: usize) -> io::Result<usize> {
//         debug_assert!(!data.is_empty());

//         if w.end > 0 {
//             w.with_filled(|staged| {
//                 self.out.extend_from_slice(staged);
//             });
//             w.end = 0;
//         }

//         let mut iov = Vec::<IoSlice<'_>>::with_capacity(data.len().saturating_sub(1) + splat);

//         if data.len() > 1 {
//             for &segment in &data[..data.len() - 1] {
//                 if !segment.is_empty() {
//                     iov.push(IoSlice::new(segment));
//                 }
//             }
//         }
//         let pattern = data[data.len() - 1];
//         for _ in 0..splat {
//             iov.push(IoSlice::new(pattern));
//         }

//         if iov.is_empty() {
//             return Ok(0);
//         }

//         let wrote = self.out.write_vectored(&iov)?;

//         let from_data = w.consume(wrote);
//         Ok(from_data)
//     }

//     fn as_any(&self) -> &dyn std::any::Any {
//         self
//     }
// }

/// A sink that forwards bytes into an arbitrary `Write` implementation.
pub struct WriteDrain<W: Write> {
    pub out: Box<W>,
}

impl<W: Write> WriteDrain<W> {
    pub fn new(writer: W) -> WriteDrain<W> {
        Self {
            out: Box::new(writer),
        }
    }
}

impl<W: Write + 'static> Drain for WriteDrain<W> {
    fn drain(&mut self, w: &mut Sink<'_>, data: &[&[u8]], splat: usize) -> io::Result<usize> {
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

        let wrote = self.out.write_vectored(&iov)?;
        if wrote == 0 && (!staged.is_empty() || splat > 0 || data.iter().any(|s| !s.is_empty())) {
            return Ok(0);
        }

        let from_data = w.consume(wrote);
        Ok(from_data)
    }

    fn flush(&mut self, w: &mut Sink<'_>) -> io::Result<()> {
        while w.end != 0 {
            let n = self.drain(w, &[&[]], 0)?;
            if n == 0 && w.end != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "drain made no progress",
                ));
            }
        }
        self.out.flush()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::Sink;
    use std::cell::RefCell;
    use std::io::IoSlice;
    use std::rc::Rc;

    #[test]
    fn write_drain_vectored_consumes_partial() {
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
        let mut writer = Sink::with_capacity(2, Box::new(WriteDrain::new(writer_impl)));
        writer.write_all(b"abcdef").unwrap();
        writer.flush().unwrap();
        assert_eq!(target.borrow().as_slice(), b"abcdef");
    }
}
