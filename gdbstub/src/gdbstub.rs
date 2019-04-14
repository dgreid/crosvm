mod gdbreply;

use std::io::Read;

fn handle_msg(msg: &[u8]) -> impl Iterator<Item = u8> {
    gdbreply::empty()
}

enum MessageState {
    Idle,
    ReceivePacket,
    ReceiveChecksum,
}

struct GdbMessage {
    packet_data: Vec<u8>, // TODO stupid extra allocation
}

#[derive(Debug)]
pub enum Error {
    ChecksumMismatch(u8, u8),
}

pub type Result<T> = std::result::Result<T, Error>;

struct GdbMessageReader<T>
where
    T: Read,
{
    source: T,
    state: MessageState,
    checksum_calculated: u8,
    data: Vec<u8>,
    msb: Option<u8>,
}

impl<T> GdbMessageReader<T>
where
    T: Read,
{
    pub fn new(source: T) -> Self {
        GdbMessageReader {
            source,
            state: MessageState::Idle,
            checksum_calculated: 0,
            data: Vec::new(),
            msb: None,
        }
    }

    /// Handles the next byte removed from the gdb client.
    /// Returns None if the message isn't yet complete.
    /// Returns a Result with either a valid message or an error on checksum failure.
    pub fn next_byte(&mut self, byte: u8) -> Option<Result<GdbMessage>> {
        use MessageState::*;

        match self.state {
            Idle => {
                if byte == b'$' {
                    self.msb = None;
                    self.checksum_calculated = 0;
                    self.state = ReceivePacket;
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
                        Some(Err(Error::ChecksumMismatch(
                            checksum_transmitted,
                            self.checksum_calculated,
                        )))
                    }
                }
            },
        }
    }
}

impl<T> Iterator for GdbMessageReader<T>
where
    T: Read,
{
    type Item = Result<GdbMessage>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut bytes = [0u8; 1];
            match self.source.read(&mut bytes) {
                Err(_) => return None, // Failure leads to dropping the connection.
                Ok(1) => (),
                Ok(_) => return None, // Also an error.
            }

            let byte = bytes[0];

            if let Some(r) = self.next_byte(bytes[0]) {
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    #[test]
    fn bad_checksum_then_good() {
        let msg = Cursor::new(b"$g#66$g#67");
        let mut messages = GdbMessageReader::new(msg);

        let result = messages.next().unwrap();
        assert!(result.is_err());
        let result = messages.next().unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn read_regs() {
        let msg = b"$g#67";
        assert_eq!(handle_msg(&msg[..]).collect::<Vec<u8>>(), b"$0000#C0");
    }
}
