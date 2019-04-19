mod gdbreply;
mod gdbsignal;

use std::io::{self, Read, Write};

pub use gdbreply::GdbReply;
pub use gdbsignal::GdbSignal;

pub struct GdbMessage {
    packet_data: Vec<u8>, // TODO stupid extra allocation
}

#[derive(Debug)]
enum ReceiveError {
    ChecksumMismatch(u8, u8),
}

type ReceiveResult<T> = std::result::Result<T, ReceiveError>;

enum MessageState {
    Idle,
    ReceivePacket,
    ReceiveChecksum,
}

struct GdbMessageReader {
    state: MessageState,
    checksum_calculated: u8,
    data: Vec<u8>,
    msn: Option<u8>,
}

impl GdbMessageReader {
    fn new() -> Self {
        GdbMessageReader {
            state: MessageState::Idle,
            checksum_calculated: 0,
            data: Vec::new(),
            msn: None,
        }
    }

    /// Handles the next byte removed from the gdb client.
    /// Returns None if the message isn't yet complete.
    /// Returns a Result with either a valid message or an error on checksum failure.
    fn next_byte(&mut self, byte: u8) -> Option<ReceiveResult<GdbMessage>> {
        use MessageState::*;

        match self.state {
            Idle => {
                if byte == b'$' {
                    self.msn = None;
                    self.checksum_calculated = 0;
                    self.state = ReceivePacket;
                    self.data.clear();
                }
                None
            }
            ReceivePacket => {
                if byte == b'#' {
                    self.state = ReceiveChecksum;
                } else {
                    self.checksum_calculated = self.checksum_calculated.wrapping_add(byte);
                    self.data.push(byte);
                    self.state = ReceivePacket;
                }
                None
            }
            ReceiveChecksum => match self.msn {
                None => {
                    self.msn = Some(byte);
                    None
                }
                Some(msn) => {
                    self.state = Idle;
                    let checksum_transmitted = from_ascii(msn) << 4 | from_ascii(byte);
                    if checksum_transmitted == self.checksum_calculated {
                        Some(Ok(GdbMessage {
                            packet_data: std::mem::replace(&mut self.data, Vec::new()),
                        }))
                    } else {
                        Some(Err(ReceiveError::ChecksumMismatch(
                            checksum_transmitted,
                            self.checksum_calculated,
                        )))
                    }
                }
            },
        }
    }
}

struct GdbMessageScanner<T>
where
    T: Read,
{
    source: T,
    reader: GdbMessageReader,
}

impl<T> GdbMessageScanner<T>
where
    T: Read,
{
    fn new(source: T) -> Self {
        GdbMessageScanner {
            source,
            reader: GdbMessageReader::new(),
        }
    }
}

impl<T> Iterator for GdbMessageScanner<T>
where
    T: Read,
{
    type Item = ReceiveResult<GdbMessage>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut bytes = [0u8; 1];
            match self.source.read(&mut bytes) {
                Err(_) => return None, // Failure leads to dropping the connection.
                Ok(1) => (),
                Ok(_) => return None, // Also an error.
            }

            if let Some(r) = self.reader.next_byte(bytes[0]) {
                return Some(r);
            }
        }
    }
}

// Returns a u8 value from an ascii digit.
fn from_ascii(digit: u8) -> u8 {
    // TODO - return an error if invalid?
    match digit {
        d if d >= b'0' && d <= b'9' => d - b'0',
        d if d >= b'a' && d <= b'f' => d - b'a',
        d if d >= b'A' && d <= b'F' => d - b'A',
        _ => 0,
    }
}

/// Errors returned by the backend.
/// `ErrorResponse` will send the u8 error code to the client.
/// `ErrorFatal` will propagate up and terminate the gdb server.
pub enum BackendError<E> {
    Response(u8),
    Fatal(E),
}
pub type BackendResult<T, E> = std::result::Result<T, BackendError<E>>;

pub enum Error<G>
where
    G: GdbBackend,
{
    WritingOutput(io::Error),
    Backend(G::Error),
}

/// Backend for a device with registers of type `Self::Register`.
/// `Register` is normally u64 or u32.
/// `Error` is the platform-specific error type to report.
pub trait GdbBackend {
    type Register;
    type Error;

    fn read_general_registers(&self) -> BackendResult<Vec<Self::Register>, Self::Error>;
    fn write_general_registers(&mut self) -> BackendResult<(), Self::Error>;
    fn cont(&mut self) -> BackendResult<GdbSignal, Self::Error>;
    fn step(&mut self) -> BackendResult<GdbSignal, Self::Error>;
    fn last_signal(&self) -> BackendResult<GdbSignal, Self::Error>;
    fn read_memory(&self, address: usize, buf: &mut [u8]) -> BackendResult<(), Self::Error>;
    fn write_memory(&mut self, address: usize, buf: &[u8]) -> BackendResult<(), Self::Error>;
    fn set_breakpoint(&mut self, address: usize) -> BackendResult<(), Self::Error>;
    fn clear_breakpoint(&mut self, address: usize) -> BackendResult<(), Self::Error>;
    fn detach(&mut self) -> BackendResult<(), Self::Error>;

    fn set_running(running: bool);

    // Hack: because I can't figure out a trait for 'implements `to_ne_bytes`'
    // TODO - avoid this slew of allocations.
    fn reg_to_ne_bytes(r: Self::Register) -> Vec<u8>;
}

pub struct GdbStub<G, W>
where
    G: GdbBackend,
    G::Register: 'static,
    W: Write,
{
    reader: GdbMessageReader,
    client_out: W,
    backend: G,
}

impl<G, W> GdbStub<G, W>
where
    G: GdbBackend,
    G::Register: 'static,
    W: Write,
{
    pub fn new(backend: G, client_out: W) -> Self {
        Self {
            backend,
            client_out,
            reader: GdbMessageReader::new(),
        }
    }

    /// Used to signal that a new byte has been received from the client.
    /// Returns 'None' if a message is not yet complete.
    pub fn byte_from_client(&mut self, byte: u8) -> std::result::Result<(), Error<G>> {
        if let Some(message_result) = self.reader.next_byte(byte) {
            match message_result {
                Ok(message) => {
                    self.client_out.write(b"+").map_err(Error::WritingOutput)?;
                    let response = handle_message(&mut self.backend, &message)?;
                    self.client_out
                        .write_all(&response.collect::<Vec<u8>>())
                        .map_err(Error::WritingOutput)?;
                }
                Err(ReceiveError::ChecksumMismatch(_, _)) => {
                    self.client_out.write(b"-").map_err(Error::WritingOutput)?;
                }
            }
        }
        Ok(())
    }
}

// Allow clippy lint to avoid
// https://www.google.com/url?q=https://github.com/rust-lang/rust-clippy/issues/4002
#[allow(clippy::redundant_closure)]
fn handle_message<G>(
    backend: &mut G,
    message: &GdbMessage,
) -> std::result::Result<Box<dyn Iterator<Item = u8>>, Error<G>>
where
    G: GdbBackend,
    G::Register: 'static,
{
    Ok(match message.packet_data[0] {
        b'g' => match backend.read_general_registers() {
            Err(BackendError::Response(e)) => Box::new(gdbreply::error(e)),
            Err(BackendError::Fatal(e)) => return Err(Error::Backend(e)),
            Ok(regs) => Box::new(GdbReply::from_bytes(
                regs.into_iter().map(|r| G::reg_to_ne_bytes(r)).flatten(),
            )),
        },
        b'G' => match backend.write_general_registers() {
            Err(BackendError::Response(e)) => Box::new(gdbreply::error(e)),
            Err(BackendError::Fatal(e)) => return Err(Error::Backend(e)),
            Ok(()) => Box::new(gdbreply::okay()),
        },
        b'c' => match backend.cont() {
            Err(BackendError::Response(e)) => Box::new(gdbreply::error(e)),
            Err(BackendError::Fatal(e)) => return Err(Error::Backend(e)),
            Ok(s) => Box::new(gdbreply::signal(s as u8)),
        },
        b's' => match backend.step() {
            Err(BackendError::Response(e)) => Box::new(gdbreply::error(e)),
            Err(BackendError::Fatal(e)) => return Err(Error::Backend(e)),
            Ok(s) => Box::new(gdbreply::signal(s as u8)),
        },
        _ => Box::new(gdbreply::error(0x00)), // TODO - replace '00' with EINVAL or ENOTSUPP?
    })
}

pub fn run_gdb_stub<S, T, G>(
    source: &mut T,
    sink: &mut S,
    backend: &mut G,
) -> std::result::Result<(), Error<G>>
where
    T: Read,
    S: Write,
    G: GdbBackend,
    G::Register: 'static,
{
    // read from source, write to the sink.
    let messages = GdbMessageScanner::new(source);

    for message in messages {
        match message {
            Err(ReceiveError::ChecksumMismatch(_, _)) => {
                sink.write(b"-").map_err(Error::WritingOutput)?;
            }
            Ok(message) => {
                sink.write(b"+").map_err(Error::WritingOutput)?;
                let reply = handle_message(backend, &message)?;
                sink.write_all(&reply.collect::<Vec<u8>>())
                    .map_err(Error::<G>::WritingOutput)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    #[test]
    fn scanner_bad_checksum_then_good() {
        let msg = Cursor::new(b"$g#66$g#67");
        let mut messages = GdbMessageScanner::new(msg);

        let result = messages.next().unwrap();
        assert!(result.is_err());
        let result = messages.next().unwrap();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().packet_data, b"g");
    }

    struct TestBackend {
        g_error: Option<u8>,
    }
    impl GdbBackend for TestBackend {
        type Register = u64;
        type Error = io::Error;

        fn read_general_registers(&self) -> BackendResult<Vec<Self::Register>, Self::Error> {
            if let Some(e) = self.g_error {
                Err(BackendError::Response(e))
            } else {
                Ok(Vec::new()) // TODO - this returns the wrong thing, should it take a better parsed message?
            }
        }

        fn write_general_registers(&mut self) -> BackendResult<(), Self::Error> {
            Ok(())
        }

        fn cont(&mut self) -> BackendResult<GdbSignal, Self::Error> {
            Ok(GdbSignal::SIGTRAP)
        }

        fn step(&mut self) -> BackendResult<GdbSignal, Self::Error> {
            Ok(GdbSignal::SIGTRAP)
        }

        fn last_signal(&self) -> BackendResult<GdbSignal, Self::Error> {
            Ok(GdbSignal::SIGTRAP)
        }
        fn read_memory(&self, address: usize, buf: &mut [u8]) -> BackendResult<(), Self::Error> {
            Ok(())
        }
        fn write_memory(&mut self, address: usize, buf: &[u8]) -> BackendResult<(), Self::Error> {
            Ok(())
        }
        fn set_breakpoint(&mut self, address: usize) -> BackendResult<(), Self::Error> {
            Ok(())
        }
        fn clear_breakpoint(&mut self, address: usize) -> BackendResult<(), Self::Error> {
            Ok(())
        }
        fn detach(&mut self) -> BackendResult<(), Self::Error> {
            Ok(())
        }
        fn set_running(running: bool) {}
        fn reg_to_ne_bytes(r: Self::Register) -> Vec<u8> {
            r.to_ne_bytes().into_iter().cloned().collect()
        }
    }

    #[test]
    fn checksum() {
        let mut input = Cursor::new(b"$g#66$g#67");
        let mut output = Cursor::new(Vec::new());
        let mut backend = TestBackend { g_error: None };

        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(&output.get_ref()[0..2], b"-+");
    }

    #[test]
    fn read_global_registers_error() {
        let mut input = Cursor::new(b"$g#67");
        let mut output = Cursor::new(Vec::new());
        let mut backend = TestBackend {
            g_error: Some(0x33),
        };
        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(output.get_ref(), b"+$E33#AB");
    }

    #[test]
    fn write_global_registers_error() {
        let mut input = Cursor::new(b"$G#47");
        let mut output = Cursor::new(Vec::new());
        let mut backend = TestBackend { g_error: None };
        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(output.get_ref(), b"+$OK#9A");
    }

    #[test]
    fn cont() {
        let mut input = Cursor::new(b"$c#63");
        let mut output = Cursor::new(Vec::new());
        let mut backend = TestBackend { g_error: None };
        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(output.get_ref(), b"+$T05#B9");
    }

    #[test]
    fn step() {
        let mut input = Cursor::new(b"$s#73");
        let mut output = Cursor::new(Vec::new());
        let mut backend = TestBackend { g_error: None };
        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(output.get_ref(), b"+$T05#B9");
    }
}
