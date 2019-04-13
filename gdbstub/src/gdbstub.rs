mod gdbreply;

fn handle_msg(msg: &[u8]) -> impl Iterator<Item = u8> {
    gdbreply::empty()
}

enum MessageState {
    Idle,
    RecievePacket(Vec<u8>),
    ReceiveChecksum(Vec<u8>, Option<u8>, Option<u8>),
}

struct GdbMessage {
    packet_data: &[u8],
}

struct MessageReader<T>
where
    T: Read,
{
    source: T,
}

impl<T> Iterator for MessageReader<T> {
    type Item = GdbMessage;

    fn next(&mut self) -> Option<Self::Item> {
        use MessageState::*;

        loop {
            let mut bytes = [0u8; 1];
            self.source.read(&mut bytes);

            let byte = bytes[0];

            match self.state {
                Idle => {
                    if byte == b'$' {
                        self.state = ReceiveChecksum(Vec::new());
                    }
                }
                ReceiveChecksum(v) => {
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_regs() {
        let msg = b"$g#67";
        assert_eq!(handle_msg(&msg[..]).collect::<Vec<u8>>(), b"$0000#C0");
    }
}
