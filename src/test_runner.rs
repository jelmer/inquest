//! Test runner utilities for managing test process I/O
//!
//! This module provides common utilities for spawning test processes and
//! managing their stdout/stderr streams with tee and parsing capabilities.

use indicatif::ProgressBar;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, SyncSender};

/// A writer that tees output to both a file and a channel
pub struct TeeWriter<W: Write> {
    writer: W,
    tx: SyncSender<Vec<u8>>,
}

impl<W: Write> TeeWriter<W> {
    pub fn new(writer: W, tx: SyncSender<Vec<u8>>) -> Self {
        TeeWriter { writer, tx }
    }
}

impl<W: Write> Write for TeeWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Write to file
        self.writer.write_all(buf)?;
        // Send to parser (ignore if receiver dropped)
        let _ = self.tx.send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// A reader that reads from a channel, buffering as needed
pub struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    pub fn new(rx: Receiver<Vec<u8>>) -> Self {
        ChannelReader {
            rx,
            buffer: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // If we have buffered data, use it first
        if self.pos < self.buffer.len() {
            let remaining = self.buffer.len() - self.pos;
            let to_copy = remaining.min(buf.len());
            buf[..to_copy].copy_from_slice(&self.buffer[self.pos..self.pos + to_copy]);
            self.pos += to_copy;
            return Ok(to_copy);
        }

        // Try to get more data from channel
        match self.rx.recv() {
            Ok(data) => {
                self.buffer = data;
                self.pos = 0;
                self.read(buf) // Recursive call to copy from new buffer
            }
            Err(_) => Ok(0), // Channel closed, EOF
        }
    }
}

/// Spawn a thread to forward stderr to the terminal via progress bar suspension
pub fn spawn_stderr_forwarder<R: Read + Send + 'static>(
    mut stderr: R,
    progress_bar: ProgressBar,
) -> std::thread::JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || -> std::io::Result<()> {
        use std::io::Write;
        let mut buffer = [0u8; 8192];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    // Write stderr output directly to stderr via progress bar suspension
                    progress_bar.suspend(|| {
                        let _ = std::io::stderr().write_all(&buffer[..n]);
                        let _ = std::io::stderr().flush();
                    });
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })
}

/// Spawn a thread to tee stdout to both storage and parsing
pub fn spawn_stdout_tee<R: Read + Send + 'static, W: Write + Send + 'static>(
    mut stdout: R,
    writer: W,
    tx: SyncSender<Vec<u8>>,
) -> std::thread::JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || -> std::io::Result<()> {
        let mut tee = TeeWriter::new(writer, tx);
        std::io::copy(&mut stdout, &mut tee)?;
        tee.flush()?;
        Ok(())
    })
}
