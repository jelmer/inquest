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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn test_tee_writer() {
        let (tx, rx) = mpsc::sync_channel(10);
        let mut file_output = Vec::new();

        {
            let mut tee = TeeWriter::new(&mut file_output, tx);
            tee.write_all(b"hello ").unwrap();
            tee.write_all(b"world").unwrap();
            tee.flush().unwrap();
        }

        // Check file output
        assert_eq!(file_output, b"hello world");

        // Check channel output
        let mut channel_output = Vec::new();
        while let Ok(data) = rx.try_recv() {
            channel_output.extend_from_slice(&data);
        }
        assert_eq!(channel_output, b"hello world");
    }

    #[test]
    fn test_channel_reader() {
        let (tx, rx) = mpsc::sync_channel(10);

        // Send data to channel
        tx.send(b"hello ".to_vec()).unwrap();
        tx.send(b"world".to_vec()).unwrap();
        drop(tx); // Close channel

        // Read from channel
        let mut reader = ChannelReader::new(rx);
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();

        assert_eq!(output, b"hello world");
    }

    #[test]
    fn test_channel_reader_buffering() {
        let (tx, rx) = mpsc::sync_channel(10);

        // Send data to channel
        tx.send(b"hello world".to_vec()).unwrap();
        drop(tx); // Close channel

        // Read in small chunks to test buffering
        let mut reader = ChannelReader::new(rx);
        let mut buf = [0u8; 3];

        // First read
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"hel");

        // Second read (should use buffered data)
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"lo ");

        // Third read (should use buffered data)
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"wor");

        // Fourth read (should use buffered data)
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"ld");

        // EOF
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn test_spawn_stdout_tee() {
        use std::sync::{Arc, Mutex};

        let (tx, rx) = mpsc::sync_channel(10);
        let file_output = Arc::new(Mutex::new(Vec::new()));
        let file_output_clone = file_output.clone();

        let input = b"test data";

        // Create a writer that wraps the Arc<Mutex<Vec>>
        struct SharedVecWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedVecWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.lock().unwrap().flush()
            }
        }

        let handle = spawn_stdout_tee(&input[..], SharedVecWriter(file_output_clone), tx);
        handle.join().unwrap().unwrap();

        // Check file output
        assert_eq!(*file_output.lock().unwrap(), b"test data");

        // Check channel output
        let mut channel_output = Vec::new();
        while let Ok(data) = rx.try_recv() {
            channel_output.extend_from_slice(&data);
        }
        assert_eq!(channel_output, b"test data");
    }

    #[test]
    fn test_spawn_stderr_forwarder() {
        // We can't easily test the actual stderr output, but we can verify
        // the thread completes successfully
        use indicatif::ProgressBar;

        let input = b"stderr data";
        let progress_bar = ProgressBar::hidden();

        let handle = spawn_stderr_forwarder(&input[..], progress_bar);
        assert!(handle.join().unwrap().is_ok());
    }
}
