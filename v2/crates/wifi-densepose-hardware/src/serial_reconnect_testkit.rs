//! Fake serial ports for reconnect supervisor tests.

use std::collections::VecDeque;
use std::io;

use super::{SerialPortFactory, SerialPortHandle, SerialReconnectConfig};

/// Scripted fake read action for serial reconnect tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeReadAction {
    /// Return these bytes from the fake port.
    Bytes(Vec<u8>),
    /// Return `Ok(0)`.
    Empty,
    /// Return an I/O error.
    Error {
        /// Error kind to return.
        kind: io::ErrorKind,
        /// Error message to return.
        message: String,
    },
}

/// Scripted fake open action for serial reconnect tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeOpenAction {
    /// Return a fake port.
    Port(FakeSerialPort),
    /// Return an I/O error.
    Error {
        /// Error kind to return.
        kind: io::ErrorKind,
        /// Error message to return.
        message: String,
    },
}

/// Scripted fake serial port.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FakeSerialPort {
    reads: VecDeque<FakeReadAction>,
}

impl FakeSerialPort {
    /// Create an empty fake port. Unscripted reads time out.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue bytes to be returned by a future read.
    pub fn push_bytes(&mut self, bytes: impl AsRef<[u8]>) {
        self.reads
            .push_back(FakeReadAction::Bytes(bytes.as_ref().to_vec()));
    }

    /// Queue a zero-length read.
    pub fn push_empty(&mut self) {
        self.reads.push_back(FakeReadAction::Empty);
    }

    /// Queue an I/O error.
    pub fn push_error(&mut self, kind: io::ErrorKind, message: impl Into<String>) {
        self.reads.push_back(FakeReadAction::Error {
            kind,
            message: message.into(),
        });
    }
}

impl SerialPortHandle for FakeSerialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.reads.pop_front() {
            Some(FakeReadAction::Bytes(bytes)) => {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                if n < bytes.len() {
                    self.reads
                        .push_front(FakeReadAction::Bytes(bytes[n..].to_vec()));
                }
                Ok(n)
            }
            Some(FakeReadAction::Empty) => Ok(0),
            Some(FakeReadAction::Error { kind, message }) => Err(io::Error::new(kind, message)),
            None => Err(io::Error::new(io::ErrorKind::TimedOut, "no scripted read")),
        }
    }
}

/// Scripted fake serial port factory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FakeSerialPortFactory {
    opens: VecDeque<FakeOpenAction>,
    open_attempts: u64,
    baud_attempts: Vec<u32>,
}

impl FakeSerialPortFactory {
    /// Create an empty fake factory. Unscripted opens return `NotFound`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a fake port to be returned by a future open.
    pub fn push_port(&mut self, port: FakeSerialPort) {
        self.opens.push_back(FakeOpenAction::Port(port));
    }

    /// Queue an I/O error to be returned by a future open.
    pub fn push_open_error(&mut self, kind: io::ErrorKind, message: impl Into<String>) {
        self.opens.push_back(FakeOpenAction::Error {
            kind,
            message: message.into(),
        });
    }

    /// Number of open attempts observed by the fake factory.
    pub fn open_attempts(&self) -> u64 {
        self.open_attempts
    }

    /// Baud rates observed for each open attempt.
    pub fn baud_attempts(&self) -> &[u32] {
        &self.baud_attempts
    }
}

impl SerialPortFactory for FakeSerialPortFactory {
    type Port = FakeSerialPort;

    fn open(&mut self, config: &SerialReconnectConfig) -> io::Result<Self::Port> {
        self.open_attempts += 1;
        self.baud_attempts.push(config.baud_rate);
        match self.opens.pop_front() {
            Some(FakeOpenAction::Port(port)) => Ok(port),
            Some(FakeOpenAction::Error { kind, message }) => Err(io::Error::new(kind, message)),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no scripted port")),
        }
    }
}
