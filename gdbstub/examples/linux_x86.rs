use std::ffi::CString;
use std::io::{stdin, stdout, Read, Write};
use std::os::raw::c_char;

use gdbstub::{DummyBackend, GdbStub};
use sys_util::Terminal;

fn show_usage(arg0: &str) {
    println!("Run the program in gdb server using stdin/out to communicate with the GDB client.");
    println!("Usage: {} program args", arg0);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 || args[1] == "--help" || args[1] == "-h" {
        show_usage(&args[0]);
        return;
    }

    let pargs = &args[1..];
    let path = CString::new(pargs[0].clone()).unwrap();
    let mut argvec: Vec<CString> = Vec::new();
    for arg in pargs[..].iter() {
        argvec.push(CString::new(arg.clone()).unwrap());
    }
    let args_p = to_exec_array(&argvec[..]);

    let stdout = stdout();
    let output = stdout.lock();
    output
        .set_raw_mode()
        .expect("Failed to set raw mode on stdout");
    let mut stub = GdbStub::new(DummyBackend::new(), output);

    let stdin = stdin();
    let input = stdin.lock();
    input
        .set_raw_mode()
        .expect("Failed to set raw mode on stdin");

    let mut test_out = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open("/tmp/gout")
        .unwrap();

    for b in input.bytes() {
        match b {
            Ok(b) => {
                test_out.write_all(&[b]).unwrap();
                match stub.byte_from_client(b) {
                    Ok(()) => (),
                    Err(_) => return,
                }
            }
            Err(_) => return,
        }
    }
    unsafe {
        // This won't return so it doesn't matter if it's safe.
        libc::execv(path.as_ptr(), args_p.as_ptr());
    }
}

fn to_exec_array(args: &[CString]) -> Vec<*const c_char> {
    use libc::c_char;
    use std::ptr;

    let mut args_p: Vec<*const c_char> = args.iter().map(|s| s.as_ptr()).collect();
    args_p.push(ptr::null());
    args_p
}
