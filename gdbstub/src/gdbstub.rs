mod gdbreply;

use std::io::{self, Read, Write};

pub use gdbreply::GdbReply;

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
/// `ErrorFatal` will propigate up and terminate the gdb server.
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
pub trait GdbBackend {
    type Register;
    type Error;

    fn read_general_registers(&self) -> BackendResult<Vec<Self::Register>, Self::Error>;
    fn write_general_registers(&mut self) -> BackendResult<(), Self::Error>;

    // TODO add other messages

    // Hack: because I can't figure out a trait for 'implements `to_ne_bytes`'
    // TODO - avoid this slew of allocations.
    fn reg_to_ne_bytes(r: Self::Register) -> Vec<u8>;
}

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
                regs.into_iter().map(|b| G::reg_to_ne_bytes(b)).flatten(),
            )),
        },
        b'G' => match backend.write_general_registers() {
            Err(BackendError::Response(e)) => Box::new(gdbreply::error(e)),
            Err(BackendError::Fatal(e)) => return Err(Error::Backend(e)),
            Ok(()) => Box::new(gdbreply::okay()),
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
}
