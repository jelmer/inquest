//! Test runner utilities for managing test process I/O
//!
//! This module provides common utilities for spawning test processes and
//! managing their stdout/stderr streams with tee and parsing capabilities.

use indicatif::ProgressBar;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

/// Shared timestamp tracking last I/O activity, stored as epoch milliseconds.
///
/// Updated by `TeeWriter` on each write. Can be polled to detect hangs.
#[derive(Debug, Clone, Default)]
pub struct ActivityTracker {
    last_activity_ms: Arc<AtomicU64>,
}

impl ActivityTracker {
    /// Create a new tracker with the current time as initial activity.
    pub fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        ActivityTracker {
            last_activity_ms: Arc::new(AtomicU64::new(now)),
        }
    }

    /// Record activity at the current time.
    pub fn touch(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_activity_ms.store(now, Ordering::Relaxed);
    }

    /// Duration since the last recorded activity.
    pub fn elapsed_since_last(&self) -> std::time::Duration {
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        std::time::Duration::from_millis(now.saturating_sub(last))
    }
}

/// A writer that tees output to both a file and a channel
pub struct TeeWriter<W: Write> {
    writer: W,
    tx: SyncSender<Vec<u8>>,
    activity: Option<ActivityTracker>,
}

impl<W: Write> TeeWriter<W> {
    /// Creates a new TeeWriter that writes to both a file and a channel.
    ///
    /// # Arguments
    /// * `writer` - The underlying writer (typically a file)
    /// * `tx` - Channel sender for broadcasting bytes to parsers
    pub fn new(writer: W, tx: SyncSender<Vec<u8>>) -> Self {
        TeeWriter {
            writer,
            tx,
            activity: None,
        }
    }

    /// Creates a new TeeWriter with an activity tracker for no-output timeout detection.
    pub fn with_activity(writer: W, tx: SyncSender<Vec<u8>>, activity: ActivityTracker) -> Self {
        TeeWriter {
            writer,
            tx,
            activity: Some(activity),
        }
    }
}

impl<W: Write> Write for TeeWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Write to file
        self.writer.write_all(buf)?;
        // Send to parser (ignore if receiver dropped)
        let _ = self.tx.send(buf.to_vec());
        // Record activity
        if let Some(ref tracker) = self.activity {
            tracker.touch();
        }
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
    /// Creates a new ChannelReader that reads from a channel.
    ///
    /// # Arguments
    /// * `rx` - Channel receiver to read bytes from
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

/// Spawn a thread to forward stderr to the terminal via progress bar suspension.
///
/// When `capture` is `Some`, each chunk read from stderr is also appended to the
/// shared buffer so the caller can recover the output after the child exits.
/// The terminal forwarding happens regardless.
///
/// Bytes are passed through an [`AnsiFilter`] before display so the child's
/// own cursor-movement and screen-erasure escapes don't fight our progress
/// bar — only colors and plain text reach the terminal. The captured copy
/// stores the *raw* bytes (post-filter would lose information that's useful
/// for `inq last` and friends).
pub fn spawn_stderr_forwarder<R: Read + Send + 'static>(
    mut stderr: R,
    progress_bar: ProgressBar,
    capture: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
) -> std::thread::JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || -> std::io::Result<()> {
        use std::io::Write;
        let mut buffer = [0u8; 8192];
        let mut filter = AnsiFilter::new();
        let mut filtered = Vec::with_capacity(8192);
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) => break, // EOF
                // A pty master returns EIO (instead of EOF) when the slave
                // has been closed and there's no more data. Treat as EOF.
                #[cfg(unix)]
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Ok(n) => {
                    let raw = &buffer[..n];
                    filtered.clear();
                    filter.filter(raw, &mut filtered);

                    // Write filtered output to stderr while the bar is
                    // suspended so it can redraw cleanly afterwards.
                    progress_bar.suspend(|| {
                        let _ = std::io::stderr().write_all(&filtered);
                        let _ = std::io::stderr().flush();
                    });
                    if let Some(ref cap) = capture {
                        if let Ok(mut buf) = cap.lock() {
                            // Capture the *raw* bytes so post-mortem tools
                            // see exactly what the child wrote.
                            buf.extend_from_slice(raw);
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })
}

/// Streaming filter that drops cursor-movement / screen-erasure ANSI escapes
/// while preserving SGR (color/style) and plain text. Used to keep the
/// child's own progress animations from interfering with our progress bar.
///
/// Operates byte-at-a-time so it survives chunk boundaries that fall inside
/// an escape sequence — common when reading from a pty master.
pub struct AnsiFilter {
    state: AnsiState,
    /// Bytes of an escape sequence currently being parsed. Flushed to the
    /// output (or dropped) once the sequence is complete.
    pending: Vec<u8>,
}

enum AnsiState {
    Plain,
    /// Just saw 0x1B (ESC); next byte determines the kind.
    Escape,
    /// Inside a CSI: `ESC [ … <final>` where final is in 0x40..=0x7E.
    Csi,
    /// Inside an OSC: `ESC ] … ( BEL | ESC \ )`.
    Osc,
    /// Inside an OSC and the previous byte was ESC; next byte completes
    /// the ST terminator if it's a backslash.
    OscEsc,
}

impl Default for AnsiFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl AnsiFilter {
    /// Build a filter in the initial "plain text" state.
    pub fn new() -> Self {
        AnsiFilter {
            state: AnsiState::Plain,
            pending: Vec::new(),
        }
    }

    /// Filter `input` into `out`. Multiple calls are stitched together so
    /// escapes spanning chunks are handled correctly.
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            self.feed(b, out);
        }
    }

    fn feed(&mut self, b: u8, out: &mut Vec<u8>) {
        match self.state {
            AnsiState::Plain => match b {
                // Drop bare carriage returns so child spinners (cargo's
                // "Building" line, pytest's progress) don't overdraw our
                // bar. CRLF terminals work fine with bare LF.
                0x0D => {}
                // Drop bell — purely an annoyance, never useful here.
                0x07 => {}
                0x1B => {
                    self.state = AnsiState::Escape;
                    self.pending.clear();
                    self.pending.push(b);
                }
                _ => out.push(b),
            },
            AnsiState::Escape => {
                self.pending.push(b);
                match b {
                    b'[' => self.state = AnsiState::Csi,
                    b']' => self.state = AnsiState::Osc,
                    // Two-byte escape sequence (no parameters). Keep
                    // everything except the screen-affecting ones.
                    b'c' | b'D' | b'E' | b'H' | b'M' => {
                        // Reset, IND, NEL, HTS, RI — drop, all touch the
                        // cursor or scroll the screen.
                        self.state = AnsiState::Plain;
                    }
                    _ => {
                        // Unknown short escape — pass through verbatim
                        // rather than corrupting the byte stream.
                        out.extend_from_slice(&self.pending);
                        self.state = AnsiState::Plain;
                    }
                }
            }
            AnsiState::Csi => {
                self.pending.push(b);
                // CSI parameter / intermediate bytes are 0x20..=0x3F;
                // final byte is 0x40..=0x7E.
                if (0x40..=0x7E).contains(&b) {
                    if b == b'm' {
                        // SGR — keep colors and styles.
                        out.extend_from_slice(&self.pending);
                    }
                    // All other CSI finals (cursor moves, erase, scroll,
                    // save/restore, etc.) — drop.
                    self.state = AnsiState::Plain;
                }
                // else: still inside parameters, keep accumulating.
            }
            AnsiState::Osc => match b {
                // BEL terminates an OSC.
                0x07 => self.state = AnsiState::Plain,
                0x1B => self.state = AnsiState::OscEsc,
                _ => {} // Drop OSC payload.
            },
            AnsiState::OscEsc => {
                if b == b'\\' {
                    // String Terminator (ESC \) closes the OSC.
                    self.state = AnsiState::Plain;
                } else {
                    // ESC inside OSC followed by something else — be
                    // conservative and keep parsing as OSC.
                    self.state = AnsiState::Osc;
                }
            }
        }
    }
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

/// Spawn a thread to tee stdout with activity tracking for no-output timeout detection
pub fn spawn_stdout_tee_tracked<R: Read + Send + 'static, W: Write + Send + 'static>(
    mut stdout: R,
    writer: W,
    tx: SyncSender<Vec<u8>>,
    activity: ActivityTracker,
) -> std::thread::JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || -> std::io::Result<()> {
        let mut tee = TeeWriter::with_activity(writer, tx, activity);
        std::io::copy(&mut stdout, &mut tee)?;
        tee.flush()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn ansi_filter(input: &[u8]) -> Vec<u8> {
        let mut filter = AnsiFilter::new();
        let mut out = Vec::new();
        filter.filter(input, &mut out);
        out
    }

    #[test]
    fn ansi_keeps_plain_text() {
        assert_eq!(ansi_filter(b"hello\nworld\n"), b"hello\nworld\n");
    }

    #[test]
    fn ansi_keeps_sgr() {
        // Red "x" reset.
        let input = b"\x1b[31mx\x1b[0m\n";
        assert_eq!(ansi_filter(input), input);
    }

    #[test]
    fn ansi_drops_carriage_return() {
        // cargo-style "spinner" overdraw becomes stacked output.
        assert_eq!(ansi_filter(b"foo\rbar\n"), b"foobar\n");
    }

    #[test]
    fn ansi_drops_erase_line() {
        // ESC [ K (erase in line) — would clear the bar's line.
        assert_eq!(ansi_filter(b"\x1b[Khello\n"), b"hello\n");
    }

    #[test]
    fn ansi_drops_cursor_movement() {
        // ESC [ 2A (up two lines), ESC [ H (home), ESC [ 1;1f (move).
        assert_eq!(ansi_filter(b"\x1b[2Ax"), b"x");
        assert_eq!(ansi_filter(b"\x1b[Hy"), b"y");
        assert_eq!(ansi_filter(b"\x1b[1;1fz"), b"z");
    }

    #[test]
    fn ansi_drops_osc() {
        // OSC for setting the window title, terminated by BEL.
        assert_eq!(ansi_filter(b"\x1b]0;title\x07hi"), b"hi");
        // OSC terminated by ESC \\ (ST).
        assert_eq!(ansi_filter(b"\x1b]0;title\x1b\\hi"), b"hi");
    }

    #[test]
    fn ansi_handles_chunk_boundary_inside_csi() {
        // Splitting "\x1b[31mX" across a chunk boundary should still
        // produce the same output.
        let mut filter = AnsiFilter::new();
        let mut out = Vec::new();
        filter.filter(b"\x1b[3", &mut out);
        filter.filter(b"1mX", &mut out);
        assert_eq!(out, b"\x1b[31mX");
    }

    #[test]
    fn ansi_keeps_unknown_short_escape() {
        // ESC = (DECKPAM) and similar — we don't recognise them but they
        // shouldn't be silently swallowed.
        assert_eq!(ansi_filter(b"\x1b=hi"), b"\x1b=hi");
    }

    #[test]
    fn ansi_drops_bell() {
        assert_eq!(ansi_filter(b"a\x07b"), b"ab");
    }

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

        let handle = spawn_stderr_forwarder(&input[..], progress_bar, None);
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn test_spawn_stderr_forwarder_capture() {
        use indicatif::ProgressBar;
        use std::sync::{Arc, Mutex};

        let input = b"captured stderr line\nsecond line\n";
        let progress_bar = ProgressBar::hidden();
        let capture = Arc::new(Mutex::new(Vec::new()));

        let handle = spawn_stderr_forwarder(&input[..], progress_bar, Some(capture.clone()));
        assert!(handle.join().unwrap().is_ok());

        let captured = capture.lock().unwrap().clone();
        assert_eq!(captured, input);
    }

    #[test]
    fn test_activity_tracker_touch_and_elapsed() {
        let tracker = ActivityTracker::new();
        // Immediately after creation, elapsed should be very small
        assert!(tracker.elapsed_since_last() < std::time::Duration::from_secs(1));

        std::thread::sleep(std::time::Duration::from_millis(50));
        let elapsed = tracker.elapsed_since_last();
        assert!(elapsed >= std::time::Duration::from_millis(50));

        // After touch, elapsed resets
        tracker.touch();
        assert!(tracker.elapsed_since_last() < std::time::Duration::from_millis(10));
    }

    #[test]
    fn test_tee_writer_with_activity_tracking() {
        let (tx, _rx) = mpsc::sync_channel(10);
        let mut file_output = Vec::new();
        let tracker = ActivityTracker::new();

        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(tracker.elapsed_since_last() >= std::time::Duration::from_millis(50));

        {
            let mut tee = TeeWriter::with_activity(&mut file_output, tx, tracker.clone());
            tee.write_all(b"data").unwrap();
        }

        // After writing, activity should be very recent
        assert!(tracker.elapsed_since_last() < std::time::Duration::from_millis(10));
    }
}
