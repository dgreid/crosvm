use std::iter::{self, IntoIterator};

enum IterState {
    Start,
    Data,
    Checksum1,
    Checksum2,
    Done,
}

pub struct GdbReply<I>
where
    I: IntoIterator<Item = u8>,
{
    data: I::IntoIter,
    checksum: u8,
    state: IterState,
}

impl<I> Iterator for GdbReply<I>
where
    I: IntoIterator<Item = u8>,
{
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        use IterState::*;

        match self.state {
            Start => {
                self.state = Data;
                Some(b'$')
            }
            Data => match self.data.next() {
                Some(x) => {
                    self.checksum = self.checksum.wrapping_add(x);
                    Some(x)
                }
                None => {
                    println!("xsum {:x}", self.checksum);
                    self.state = Checksum1;
                    Some(b'#')
                }
            },
            Checksum1 => {
                self.state = Checksum2;
                Some(hex_msn(self.checksum))
            }
            Checksum2 => {
                self.state = Done;
                Some(hex_lsn(self.checksum))
            }
            Done => None,
        }
    }
}

impl<T> GdbReply<T>
where
    T: IntoIterator<Item = u8>,
{
    pub(crate) fn from_bytes(data: T) -> Self {
        GdbReply {
            data: data.into_iter(),
            state: IterState::Start,
            checksum: 0,
        }
    }
}

pub fn empty() -> GdbReply<iter::Empty<u8>> {
    GdbReply::from_bytes(iter::empty())
}

pub struct GdbErrorData {
    errno: u8,
    idx: usize,
}

impl Iterator for GdbErrorData {
    type Item = u8;
    fn next(&mut self) -> Option<u8> {
        let ret = match self.idx {
            0 => Some(b'E'),
            1 => Some(hex_msn(self.errno)),
            2 => Some(hex_lsn(self.errno)),
            _ => None,
        };
        self.idx = self.idx.saturating_add(1);
        ret
    }
}

pub fn error(errno: u8) -> GdbReply<GdbErrorData> {
    GdbReply::from_bytes(GdbErrorData { errno, idx: 0 })
}

pub struct GdbOkData {
    idx: usize,
}

impl Iterator for GdbOkData {
    type Item = u8;
    fn next(&mut self) -> Option<u8> {
        let ret = match self.idx {
            0 => Some(b'O'),
            1 => Some(b'K'),
            _ => None,
        };
        self.idx = self.idx.saturating_add(1);
        ret
    }
}

pub fn okay() -> GdbReply<GdbOkData> {
    GdbReply::from_bytes(GdbOkData { idx: 0 })
}

pub struct GdbLastSignal {
    idx: usize,
    signal: u8,
}

impl Iterator for GdbLastSignal {
    type Item = u8;
    fn next(&mut self) -> Option<u8> {
        let ret = match self.idx {
            0 => Some(b'T'),
            1 => Some(hex_msn(self.signal)),
            2 => Some(hex_lsn(self.signal)),
            _ => None,
        };
        self.idx = self.idx.saturating_add(1);
        ret
    }
}

pub fn signal(signal: u8) -> GdbReply<GdbLastSignal> {
    GdbReply::from_bytes(GdbLastSignal { idx: 0, signal })
}

fn ascii_byte(digit: u8) -> u8 {
    match digit {
        d if d < 0xa => d + b'0',
        d if d <= 0xf => d - 0xa + b'A',
        _ => b'0',
    }
}
fn hex_lsn(num: u8) -> u8 {
    ascii_byte(num & 0x0f)
}
fn hex_msn(num: u8) -> u8 {
    ascii_byte((num >> 4) & 0x0f)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_replies() {
        assert_eq!(error(0x55).collect::<Vec<u8>>(), b"$E55#AF");
        assert_eq!(signal(0xaa).collect::<Vec<u8>>(), b"$TAA#D6");
    }
}
