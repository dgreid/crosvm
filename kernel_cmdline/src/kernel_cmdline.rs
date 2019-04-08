// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helper for creating valid kernel command line strings.

use std::fmt::{self, Display};
use std::result;

/// The error type for command line building operations.
#[derive(PartialEq, Debug)]
pub enum Error {
    /// Operation would have resulted in a non-printable ASCII character.
    InvalidAscii,
    /// Key/Value Operation would have had a space in it.
    HasSpace,
    /// Key/Value Operation would have had an equals sign in it.
    HasEquals,
    /// Operation would have made the command line too large.
    TooLarge,
    /// Adding kernel args after init args is not allowed.
    InitArgsAdded,
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        let description = match self {
            InvalidAscii => "string contains non-printable ASCII character",
            HasSpace => "string contains a space",
            HasEquals => "string contains an equals sign",
            TooLarge => "inserting string would make command line too long",
            InitArgsAdded => "inserting kernel args after init args isn't allowed",
        };

        write!(f, "{}", description)
    }
}

/// Specialized Result type for command line operations.
pub type Result<T> = result::Result<T, Error>;

fn valid_char(c: char) -> bool {
    match c {
        ' '...'~' => true,
        _ => false,
    }
}

fn valid_str(s: &str) -> Result<()> {
    if s.chars().all(valid_char) {
        Ok(())
    } else {
        Err(Error::InvalidAscii)
    }
}

fn valid_element(s: &str) -> Result<()> {
    if !s.chars().all(valid_char) {
        Err(Error::InvalidAscii)
    } else if s.contains(' ') {
        Err(Error::HasSpace)
    } else if s.contains('=') {
        Err(Error::HasEquals)
    } else {
        Ok(())
    }
}

/// A builder for a kernel command line string that validates the string as its being built. A
/// `CString` can be constructed from this directly using `CString::new`.
pub struct Cmdline {
    line: String,
    capacity: usize,
    has_init_args: bool,
}

impl Cmdline {
    /// Constructs an empty Cmdline with the given capacity, which includes the nul terminator.
    /// Capacity must be greater than 0.
    pub fn new(capacity: usize) -> Cmdline {
        assert_ne!(capacity, 0);
        Cmdline {
            line: String::new(),
            capacity,
            has_init_args: false,
        }
    }

    fn has_capacity(&self, more: usize) -> Result<()> {
        let needs_space = if self.line.is_empty() { 0 } else { 1 };
        if self.line.len() + more + needs_space < self.capacity {
            Ok(())
        } else {
            Err(Error::TooLarge)
        }
    }

    fn start_push(&mut self) {
        if !self.line.is_empty() {
            self.line.push(' ');
        }
    }

    fn end_push(&mut self) {
        // This assert is always true because of the `has_capacity` check that each insert method
        // uses.
        assert!(self.line.len() < self.capacity);
    }

    /// Validates and inserts a key value pair into this command line
    pub fn insert<T: AsRef<str>>(&mut self, key: T, val: T) -> Result<()> {
        if self.has_init_args {
            return Err(Error::InitArgsAdded);
        }

        let k = key.as_ref();
        let v = val.as_ref();

        valid_element(k)?;
        valid_element(v)?;
        self.has_capacity(k.len() + v.len() + 1)?;

        self.start_push();
        self.line.push_str(k);
        self.line.push('=');
        self.line.push_str(v);
        self.end_push();

        Ok(())
    }

    /// Validates and inserts a string to the end of the current command line
    pub fn insert_str<T: AsRef<str>>(&mut self, slug: T) -> Result<()> {
        if self.has_init_args {
            return Err(Error::InitArgsAdded);
        }

        self.insert_str_internal(slug)
    }

    /// Appends the given string as an argument to be passed to init.
    /// These arguments are appended to the command line after a '--' to indicate they are passed
    /// through to init. After init args are added, it is invalid to add more kernel args.
    pub fn insert_init_arg<T: AsRef<str>>(&mut self, arg: T) -> Result<()> {
        if !self.has_init_args {
            self.insert_str("--")?;
            self.has_init_args = true;
        }
        self.insert_str_internal(arg)
    }

    /// Returns the cmdline in progress without nul termination
    pub fn as_str(&self) -> &str {
        self.line.as_str()
    }

    // Validates and inserts a string to the end of the current command line
    fn insert_str_internal<T: AsRef<str>>(&mut self, slug: T) -> Result<()> {
        let s = slug.as_ref();
        valid_str(s)?;

        self.has_capacity(s.len())?;

        self.start_push();
        self.line.push_str(s);
        self.end_push();

        Ok(())
    }
}

impl Into<Vec<u8>> for Cmdline {
    fn into(self) -> Vec<u8> {
        self.line.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn insert_hello_world() {
        let mut cl = Cmdline::new(100);
        assert_eq!(cl.as_str(), "");
        assert!(cl.insert("hello", "world").is_ok());
        assert_eq!(cl.as_str(), "hello=world");

        let s = CString::new(cl).expect("failed to create CString from Cmdline");
        assert_eq!(s, CString::new("hello=world").unwrap());
    }

    #[test]
    fn insert_multi() {
        let mut cl = Cmdline::new(100);
        assert!(cl.insert("hello", "world").is_ok());
        assert!(cl.insert("foo", "bar").is_ok());
        assert_eq!(cl.as_str(), "hello=world foo=bar");
    }

    #[test]
    fn insert_space() {
        let mut cl = Cmdline::new(100);
        assert_eq!(cl.insert("a ", "b"), Err(Error::HasSpace));
        assert_eq!(cl.insert("a", "b "), Err(Error::HasSpace));
        assert_eq!(cl.insert("a ", "b "), Err(Error::HasSpace));
        assert_eq!(cl.insert(" a", "b"), Err(Error::HasSpace));
        assert_eq!(cl.as_str(), "");
    }

    #[test]
    fn insert_equals() {
        let mut cl = Cmdline::new(100);
        assert_eq!(cl.insert("a=", "b"), Err(Error::HasEquals));
        assert_eq!(cl.insert("a", "b="), Err(Error::HasEquals));
        assert_eq!(cl.insert("a=", "b "), Err(Error::HasEquals));
        assert_eq!(cl.insert("=a", "b"), Err(Error::HasEquals));
        assert_eq!(cl.insert("a", "=b"), Err(Error::HasEquals));
        assert_eq!(cl.as_str(), "");
    }

    #[test]
    fn insert_emoji() {
        let mut cl = Cmdline::new(100);
        assert_eq!(cl.insert("heart", "💖"), Err(Error::InvalidAscii));
        assert_eq!(cl.insert("💖", "love"), Err(Error::InvalidAscii));
        assert_eq!(cl.as_str(), "");
    }

    #[test]
    fn insert_string() {
        let mut cl = Cmdline::new(13);
        assert_eq!(cl.as_str(), "");
        assert!(cl.insert_str("noapic").is_ok());
        assert_eq!(cl.as_str(), "noapic");
        assert!(cl.insert_str("nopci").is_ok());
        assert_eq!(cl.as_str(), "noapic nopci");
    }

    #[test]
    fn insert_too_large() {
        let mut cl = Cmdline::new(4);
        assert_eq!(cl.insert("hello", "world"), Err(Error::TooLarge));
        assert_eq!(cl.insert("a", "world"), Err(Error::TooLarge));
        assert_eq!(cl.insert("hello", "b"), Err(Error::TooLarge));
        assert!(cl.insert("a", "b").is_ok());
        assert_eq!(cl.insert("a", "b"), Err(Error::TooLarge));
        assert_eq!(cl.insert_init_arg("a"), Err(Error::TooLarge));
        assert_eq!(cl.insert_str("a"), Err(Error::TooLarge));
        assert_eq!(cl.as_str(), "a=b");

        let mut cl = Cmdline::new(10);
        assert!(cl.insert("ab", "ba").is_ok()); // adds 5 length
        assert_eq!(cl.insert("c", "da"), Err(Error::TooLarge)); // adds 5 (including space) length
        assert!(cl.insert("c", "d").is_ok()); // adds 4 (including space) length
    }

    #[test]
    fn init_params_first() {
        let mut cl = Cmdline::new(255);
        assert!(cl.insert("a", "b").is_ok());
        assert!(cl.insert_init_arg("hello_init").is_ok());
        assert_eq!(cl.insert("hello", "world"), Err(Error::InitArgsAdded));
        assert_eq!(cl.as_str(), "a=b -- hello_init");
    }
}
