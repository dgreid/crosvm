mod gdbreply;
mod gdbsignal;

use std::io::{self, Read, Write};
use std::iter;

pub use gdbreply::{hex_lsn, hex_msn, GdbReply};
pub use gdbsignal::GdbSignal;

pub struct GdbMessage {
    packet_data: Vec<u8>, // TODO stupid extra allocation
}

#[derive(Debug)]
enum ReceiveError {
    ChecksumMismatch(u8, u8),
    InvalidDigit,
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
                    let checksum_transmitted =
                        from_ascii(msn).unwrap_or(0) << 4 | from_ascii(byte).unwrap_or(0);
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
fn from_ascii(digit: u8) -> ReceiveResult<u8> {
    // TODO - return an error if invalid?
    match digit {
        d if d >= b'0' && d <= b'9' => Ok(d - b'0'),
        d if d >= b'a' && d <= b'f' => Ok(0xa + d - b'a'),
        d if d >= b'A' && d <= b'F' => Ok(0xa + d - b'A'),
        _ => Err(ReceiveError::InvalidDigit),
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
    pub fn byte_from_client(
        &mut self,
        byte: u8,
        f: &mut std::fs::File,
    ) -> std::result::Result<(), Error<G>> {
        if let Some(message_result) = self.reader.next_byte(byte) {
            match message_result {
                Ok(message) => {
                    //                    f.write_all(b"\n\ngot message\n").unwrap();
                    self.client_out.write(b"+").map_err(Error::WritingOutput)?;
                    f.write(b"+").unwrap();
                    self.client_out.flush();

                    let response = handle_message(&mut self.backend, &message)?;
                    let response_vec = &response.collect::<Vec<u8>>();
                    self.client_out
                        .write_all(&response_vec)
                        .map_err(Error::WritingOutput)?;
                    f.write_all(&response_vec).map_err(Error::WritingOutput)?;
                }
                Err(ReceiveError::ChecksumMismatch(t, c)) => {
                    //write!(f, "\n\ngot bad checksum t:{:x} c:{:x}\n", t, c).unwrap();
                    self.client_out.write(b"-").map_err(Error::WritingOutput)?;
                    f.write(b"-").unwrap();
                }
                Err(ReceiveError::InvalidDigit) => {
                    //write!(f, "\n\ngot bad digit t:{:x} c:{:x}\n", t, c).unwrap();
                    self.client_out.write(b"-").map_err(Error::WritingOutput)?;
                    f.write(b"-").unwrap();
                }
            }
            self.client_out.flush();
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
        b'?' => handle_reason_stopped(backend, message),
        b'c' => match backend.cont() {
            Err(BackendError::Response(e)) => Box::new(gdbreply::error(e)),
            Err(BackendError::Fatal(e)) => return Err(Error::Backend(e)),
            Ok(s) => Box::new(gdbreply::signal(s as u8)),
        },
        b'g' => match backend.read_general_registers() {
            Err(BackendError::Response(e)) => Box::new(gdbreply::error(e)),
            Err(BackendError::Fatal(e)) => return Err(Error::Backend(e)),
            Ok(regs) => Box::new(GdbReply::from_bytes(
                regs.into_iter()
                    .map(|r| G::reg_to_ne_bytes(r))
                    .flatten()
                    .flat_map(|r| iter::once(hex_msn(r)).chain(iter::once(hex_lsn(r)))),
            )),
        },
        b'G' => match backend.write_general_registers() {
            Err(BackendError::Response(e)) => Box::new(gdbreply::error(e)),
            Err(BackendError::Fatal(e)) => return Err(Error::Backend(e)),
            Ok(()) => Box::new(gdbreply::okay()),
        },
        b'H' => handle_commandH(backend, message),
        b'm' => handle_read_memory(backend, message),
        b'q' => Box::new(GdbReply::from_bytes(b"".to_vec())), //TODO - actual response
        b's' => match backend.step() {
            Err(BackendError::Response(e)) => Box::new(gdbreply::error(e)),
            Err(BackendError::Fatal(e)) => return Err(Error::Backend(e)),
            Ok(s) => Box::new(gdbreply::signal(s as u8)),
        },
        _ => {
            // The gdb spec seems to be to reply empty for unsupported commands.
            Box::new(gdbreply::empty())
        }
    })
}

fn hex_to_u64<T>(bytes: T) -> ReceiveResult<u64>
where
    T: IntoIterator<Item = u8>,
{
    bytes
        .into_iter()
        .map(|addr_byte| from_ascii(addr_byte))
        .collect::<ReceiveResult<Vec<_>>>()
        .map(|nibbles| {
            nibbles
                .iter()
                .take(16) // Max of 16 hex digits in a u64.
                .fold(0u64, |a, n| (a << 4) | (u64::from(*n) & 0x0f))
        })
}

fn handle_read_memory<G>(backend: &mut G, message: &GdbMessage) -> Box<dyn Iterator<Item = u8>>
where
    G: GdbBackend,
    G::Register: 'static,
{
    // The format for read-memory is "mAA..AA,LL..LL" where AA..AA is the address to read from, and
    // LL..LL is the length to read in bytes.
    let message_data = &message.packet_data[1..];
    let message_bytes = message_data.iter();

    let address = match hex_to_u64(message_bytes.take_while(|b| **b != b',').cloned()) {
        Ok(a) => a,
        Err(_) => return Box::new(gdbreply::error(1)),
    };

    Box::new(gdbreply::okay())
}

fn handle_reason_stopped<G>(backend: &mut G, message: &GdbMessage) -> Box<dyn Iterator<Item = u8>>
where
    G: GdbBackend,
    G::Register: 'static,
{
    // if is stopped
    Box::new(GdbReply::from_bytes(b"S05".to_vec()))
    // else when running...
}

fn handle_commandH<G>(backend: &mut G, message: &GdbMessage) -> Box<dyn Iterator<Item = u8>>
where
    G: GdbBackend,
    G::Register: 'static,
{
    let message_data = &message.packet_data[1..];
    match message_data[0] {
        b'g' => Box::new(gdbreply::okay()),
        _ => Box::new(gdbreply::empty()),
    }
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
            Err(ReceiveError::InvalidDigit) | Err(ReceiveError::ChecksumMismatch(_, _)) => {
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

pub struct DummyBackend {
    g_error: Option<u8>,
}
impl DummyBackend {
    pub fn new() -> DummyBackend {
        DummyBackend { g_error: None }
    }
}

impl GdbBackend for DummyBackend {
    type Register = u32;
    type Error = io::Error;

    fn read_general_registers(&self) -> BackendResult<Vec<Self::Register>, Self::Error> {
        if let Some(e) = self.g_error {
            Err(BackendError::Response(e))
        } else {
            Ok(vec![0; 16])
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

    #[test]
    fn checksum() {
        let mut input = Cursor::new(b"$g#66$g#67");
        let mut output = Cursor::new(Vec::new());
        let mut backend = DummyBackend { g_error: None };

        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(&output.get_ref()[0..2], b"-+");
    }

    #[test]
    fn read_global_registers_error() {
        let mut input = Cursor::new(b"$g#67");
        let mut output = Cursor::new(Vec::new());
        let mut backend = DummyBackend {
            g_error: Some(0x33),
        };
        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(output.get_ref(), b"+$E33#AB");
    }

    #[test]
    fn write_global_registers_error() {
        let mut input = Cursor::new(b"$G#47");
        let mut output = Cursor::new(Vec::new());
        let mut backend = DummyBackend { g_error: None };
        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(output.get_ref(), b"+$OK#9A");
    }

    #[test]
    fn cont() {
        let mut input = Cursor::new(b"$c#63");
        let mut output = Cursor::new(Vec::new());
        let mut backend = DummyBackend { g_error: None };
        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(output.get_ref(), b"+$T05#B9");
    }

    #[test]
    fn step() {
        let mut input = Cursor::new(b"$s#73");
        let mut output = Cursor::new(Vec::new());
        let mut backend = DummyBackend { g_error: None };
        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(output.get_ref(), b"+$T05#B9");
    }

    #[test]
    fn q_packet() {
        let input =
            Cursor::new(b"$qSupported:multiprocess+;swbreak+;hwbreak+;qRelocInsn+;fork-events+;vfork-events+;exec-events+;vContSupported+;QThreadEvents+;no-resumed+;xmlRegisters=i386#6a".to_vec());
        let mut messages = GdbMessageScanner::new(input);
        let result = messages.next().unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn g_packet() {
        let mut input = Cursor::new(b"$g#67".to_vec());
        let mut output = Cursor::new(Vec::new());
        let mut backend = DummyBackend { g_error: None };
        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(
            output.get_ref(),
            &b"+$00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000#00".to_vec()
        );
    }

    #[test]
    fn receive_bytes() {
        let input = b"0123456789abcdef".to_vec();
        assert_eq!(0x0123_4567_89ab_cdef, hex_to_u64(input).unwrap());

        let invalid = b"2q2".to_vec();
        assert!(hex_to_u64(invalid).is_err());

        // consume at most a u64 worth of digits.
        let too_long = b"12341234123412345".to_vec();
        assert_eq!(0x1234_1234_1234_1234, hex_to_u64(too_long).unwrap());
    }
}
