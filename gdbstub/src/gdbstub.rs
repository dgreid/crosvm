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
    msb: Option<u8>,
}

impl GdbMessageReader {
    fn new() -> Self {
        GdbMessageReader {
            state: MessageState::Idle,
            checksum_calculated: 0,
            data: Vec::new(),
            msb: None,
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
                    self.msb = None;
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
            ReceiveChecksum => match self.msb {
                None => {
                    self.msb = Some(byte);
                    None
                }
                Some(msb) => {
                    self.state = Idle;
                    let checksum_transmitted = from_ascii(msb) << 4 | from_ascii(byte);
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

// Return a u8 value from an ascii digit.
fn from_ascii(digit: u8) -> u8 {
    // TODO - return an error if invalid?
    match digit {
        d if d >= b'0' && d <= b'9' => d - b'0',
        d if d >= b'a' && d <= b'f' => d - b'a',
        d if d >= b'A' && d <= b'F' => d - b'A',
        _ => 0,
    }
}

pub enum Error<G>
where
    G: GdbBackend,
{
    WritingOutput(io::Error),
    BackendError(G::Error),
}

/// Backend for a device with registers of type `Self::Register` and an error
/// type of `Self::Error`.
/// `Register` is normally u64 or u32.
pub trait GdbBackend {
    type Error;
    type Register;

    fn read_general_registers(&self) -> std::result::Result<Vec<Self::Register>, Self::Error>;
    // TODO add other messages
}

fn handle_message<G>(
    backend: &mut G,
    message: &GdbMessage,
) -> std::result::Result<Vec<u8>, Error<G>>
where
    G: GdbBackend,
{
    let reply = match message.packet_data[0] {
        b'g' => Vec::new(),
        _ => b"E00".to_vec(), // TODO - replace '00' with EINVAL or ENOTSUPP?
    };
    Ok(reply)
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
                sink.write(&reply).map_err(Error::<G>::WritingOutput)?;
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

    struct TestBackend {}
    impl GdbBackend for TestBackend {
        type Error = std::io::Error;
        type Register = u64;

        fn read_general_registers(&self) -> std::result::Result<Vec<Self::Register>, Self::Error> {
            Ok(Vec::new()) // TODO - this returns the wrong thing, should it take a better parsed message?
        }
    }

    #[test]
    fn handle_message() {
        let mut input = Cursor::new(b"$g#66$g#67");
        let mut output = Cursor::new(Vec::new());
        let mut backend = TestBackend {};

        assert!(run_gdb_stub(&mut input, &mut output, &mut backend).is_ok());
        assert_eq!(output.get_ref(), b"-+TODO");
    }
}
