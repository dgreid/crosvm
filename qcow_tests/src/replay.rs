extern crate qcow;

fn main() {
    let mut args = std::env::args();
    if args.next().is_none() {
        println!("expected executable name");
        return;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(args.next().unwrap())
        .unwrap();
    qcow::QcowHeader::create_for_size(1024*1024*1024*256)
        .write_to(&mut file)
        .unwrap();

    let qcow = qcow::QcowFile::from(file).unwrap();

    let path_buf = std::path::PathBuf::from(args.next().unwrap());
    qcow::playback_actions(qcow, &path_buf);
}
