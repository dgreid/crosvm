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
                    self.checksum.wrapping_add(x);
                    Some(x)
                }
                None => {
                    self.state = Checksum1;
                    Some(b'#')
                }
            },
            Checksum1 => {
                self.state = Checksum2;
                Some(hex_msb(self.checksum))
            }
            Checksum2 => {
                self.state = Done;
                Some(hex_lsb(self.checksum))
            }
            Done => None,
        }
    }
}

impl<T> GdbReply<T>
where
    T: IntoIterator<Item = u8>,
{
    fn new(data: T) -> Self {
        GdbReply {
            data: data.into_iter(),
            state: IterState::Start,
            checksum: 0,
        }
    }
}

pub fn empty() -> GdbReply<iter::Empty<u8>> {
    GdbReply::new(iter::empty())
}

/*
    /// Sends the GDB reply to the passed write target.
    pub fn Send(&self) -> std::io::Result<()> {
        Ok(())
    }
}
*/

pub struct GdbErrorData {
    errno: u8,
    idx: usize,
}

impl Iterator for GdbErrorData {
    type Item = u8;
    fn next(&mut self) -> Option<u8> {
        let ret = match self.idx {
            0 => Some(b'e'),
            1 => Some(hex_msb(self.errno)),
            2 => Some(hex_lsb(self.errno)),
            _ => None,
        };
        self.idx = self.idx.saturating_add(1);
        ret
    }
}

pub fn error(errno: u8) -> GdbReply<GdbErrorData> {
    GdbReply::new(GdbErrorData { errno, idx: 0 })
}

fn ascii_byte(digit: u8) -> u8 {
    match digit {
        d if d <= 9 => d + b'0',
        d if d <= 0xf => d + b'a',
        _ => b'0',
    }
}
fn hex_lsb(digit: u8) -> u8 {
    ascii_byte(digit & 0x0f)
}
fn hex_msb(digit: u8) -> u8 {
    ascii_byte(digit & 0xf0 >> 4)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        assert_eq!(empty().collect::<Vec<u8>>(), b"$#00");
        assert_eq!(error(0x55).collect::<Vec<u8>>(), b"$e55#00");
    }
}
