#![feature(core_io_borrowed_buf)]
#![feature(read_buf)]
#![feature(maybe_uninit_slice)]
#![feature(maybe_uninit_write_slice)]
#![feature(file_buffered)]

use rustio::{StackBuffer, WritableStream};
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn memory_demo() -> io::Result<()> {
    let mut buffer = StackBuffer::<4096>::new();
    let mut stream = buffer.writable_stream(Vec::new());

    {
        let w = stream.writer();
        w.write_all(b"hello ")?;
        w.write_all(b"world")?;

        w.with_filled(|bytes| {
            println!(
                "staged before flush: {:?}",
                std::str::from_utf8(bytes).unwrap()
            );
        });

        w.flush()?;
    }
    println!(
        "after first flush -> {}",
        std::str::from_utf8(stream.sink().as_slice()).unwrap()
    );

    {
        let w = stream.writer();
        let mut cursor = Cursor::new(b" + from reader");
        let n_read = w.with_unfilled(|buf| {
            let before = buf.len();
            cursor.read_buf(buf.unfilled())?;
            Ok::<_, io::Error>(buf.len() - before)
        })?;
        println!("read into tail: {n_read} bytes");

        w.splat_all(b"!", 3)?;
        w.flush()?;
    }

    println!(
        "after second flush -> {}",
        std::str::from_utf8(stream.sink().as_slice()).unwrap()
    );
    Ok(())
}

fn stack_reuse_demo() -> io::Result<()> {
    let mut storage = StackBuffer::<64>::new();

    {
        let mut stream = WritableStream::with_buffer(&mut storage, Vec::new());
        stream.writer().write_all(b"stack first run")?;
        stream.writer().flush()?;
        println!(
            "first run -> {}",
            std::str::from_utf8(stream.sink().as_slice()).unwrap()
        );
    }

    {
        let mut stream = WritableStream::with_buffer(&mut storage, Vec::new());
        stream.writer().write_all(b"stack reuse")?;
        stream.writer().flush()?;
        println!(
            "reuse -> {}",
            std::str::from_utf8(stream.sink().as_slice()).unwrap()
        );
    }
    Ok(())
}

fn stdout_demo() -> io::Result<()> {
    let mut buffer = [0u8; 256];

    {
        let mut stdout = WritableStream::with_slice(&mut buffer, io::stdout());
        stdout.writer().write_all(b"stack-backed logger\n")?;
        stdout.writer().flush()?;
    }

    {
        let mut buffer = StackBuffer::<8>::new();
        let mut stdout = buffer.writable_stream(io::stdout());

        stdout.writer().write_all(b"allocating logger\n")?;
        stdout.writer().flush()?;
    }

    Ok(())
}

fn file_demo() -> io::Result<()> {
    let mut pathbuf = PathBuf::from(std::env::temp_dir());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    pathbuf.push(format!("rustio-demo-{now}.log"));
    let path = pathbuf.as_path();

    {
        let file = File::create_new(path)?;
        let mut buffer = StackBuffer::<8>::new();
        let mut file_logger = buffer.writable_stream(file);

        let w = file_logger.writer();
        w.write_all(b"file logger demo\n")?;
        w.write_all(b"log entry 1\n")?;
        w.splat_all(b"*", 10)?;
        w.write_all(b"\n")?;
        w.flush()?;
    }

    let mut contents = String::new();
    let n = File::open_buffered(path)?.read_to_string(&mut contents)?;

    println!("[file] wrote {} bytes to {}", n, path.display());
    print!("[file] contents:\n{}", contents);
    std::fs::remove_file(path).ok();
    Ok(())
}

fn main() -> io::Result<()> {
    println!("== Memory collector demo ==");
    memory_demo()?;
    println!();

    println!("== Stack reuse demo ==");
    stack_reuse_demo()?;
    println!();

    println!("== Stdout logger demo ==");
    stdout_demo()?;
    println!();

    println!("== File logger demo ==");
    file_demo()?;

    Ok(())
}
