mod gdbreply;

fn handle_msg(msg: &[u8]) -> impl Iterator<Item = u8> {
    gdbreply::empty()
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
