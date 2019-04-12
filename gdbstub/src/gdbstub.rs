use std::iter::IntoIterator;

enum IterState {
    Start,
    Data,
    Checksum,
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
                    self.state = Checksum;
                    Some(b'#')
                }
            },
            Checksum => {
                self.state = Done;
                Some(self.checksum)
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

pub fn empty() -> GdbReply<Vec<u8>> {
    GdbReply::new(Vec::new())
}

/*
    /// Sends the GDB reply to the passed write target.
    pub fn Send(&self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn error(errno: u8) -> GdbReply {
    GdbMessage { data: "${}#" }
}
*/

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
