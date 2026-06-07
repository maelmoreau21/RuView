//! Feature-gated serial hot-plug supervisor.
//!
//! The supervisor owns one blocking serial handle at a time. Read failures and
//! zero-length reads are treated as disconnect signals; the next poll reopens
//! the port through the configured factory and reports exponential backoff
//! delays to the caller.

use std::cmp;
use std::io;
use std::time::Duration;

use thiserror::Error;

const MAX_PORT_NAME_LEN: usize = 128;
/// Default ESP32 serial baud probing order.
pub const DEFAULT_BAUD_CANDIDATES: [u32; 3] = [115_200, 460_800, 921_600];

/// Blocking serial handle used by the reconnect supervisor.
pub trait SerialPortHandle: Send {
    /// Read bytes from the port into `buf`.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

impl SerialPortHandle for Box<dyn serialport::SerialPort> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        std::io::Read::read(self, buf)
    }
}

/// Factory for opening serial handles.
pub trait SerialPortFactory {
    /// Concrete port type returned by this factory.
    type Port: SerialPortHandle;

    /// Open the serial port described by `config`.
    fn open(&mut self, config: &SerialReconnectConfig) -> io::Result<Self::Port>;
}

/// Production serial factory backed by the `serialport` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemSerialPortFactory;

impl SerialPortFactory for SystemSerialPortFactory {
    type Port = Box<dyn serialport::SerialPort>;

    fn open(&mut self, config: &SerialReconnectConfig) -> io::Result<Self::Port> {
        serialport::new(&config.port_name, config.baud_rate)
            .timeout(config.read_timeout)
            .open()
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))
    }
}

/// Configuration for serial reconnect supervision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialReconnectConfig {
    /// OS serial port name, e.g. `COM12` or `/dev/ttyUSB0`.
    pub port_name: String,
    /// Last configured or successfully opened serial baud rate.
    pub baud_rate: u32,
    /// Ordered serial baud rates probed when opening the port.
    pub baud_candidates: Vec<u32>,
    /// Per-read timeout configured on the underlying serial port.
    pub read_timeout: Duration,
    /// First reconnect delay after an open or read failure.
    pub initial_backoff: Duration,
    /// Maximum reconnect delay after repeated failures.
    pub max_backoff: Duration,
    /// Scratch buffer size for each successful read event.
    pub read_buffer_size: usize,
}

impl SerialReconnectConfig {
    /// Create a config with conservative ESP32-friendly defaults.
    pub fn new(port_name: impl Into<String>, baud_rate: u32) -> Self {
        Self {
            port_name: port_name.into(),
            baud_rate,
            baud_candidates: Self::default_baud_candidates_for(baud_rate),
            read_timeout: Duration::from_millis(250),
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
            read_buffer_size: 2048,
        }
    }

    /// Override the ordered baud rates probed when opening the serial port.
    pub fn with_baud_candidates(mut self, baud_candidates: impl IntoIterator<Item = u32>) -> Self {
        self.baud_candidates = Self::normalize_baud_candidates(baud_candidates);
        if let Some(&baud_rate) = self.baud_candidates.first() {
            self.baud_rate = baud_rate;
        }
        self
    }

    /// Ordered baud rates probed when opening the serial port.
    pub fn baud_candidates(&self) -> &[u32] {
        &self.baud_candidates
    }

    /// Validate caller-provided serial settings.
    pub fn validate(&self) -> Result<(), SerialReconnectError> {
        if self.port_name.trim().is_empty() {
            return Err(SerialReconnectError::InvalidConfig {
                field: "port_name",
                reason: "must not be empty",
            });
        }
        if self.port_name.len() > MAX_PORT_NAME_LEN {
            return Err(SerialReconnectError::InvalidConfig {
                field: "port_name",
                reason: "must be 128 bytes or shorter",
            });
        }
        if self.port_name.chars().any(char::is_control) {
            return Err(SerialReconnectError::InvalidConfig {
                field: "port_name",
                reason: "must not contain control characters",
            });
        }
        if self.baud_rate == 0 {
            return Err(SerialReconnectError::InvalidConfig {
                field: "baud_rate",
                reason: "must be greater than zero",
            });
        }
        if self.baud_candidates.is_empty() {
            return Err(SerialReconnectError::InvalidConfig {
                field: "baud_candidates",
                reason: "must contain at least one baud rate",
            });
        }
        if self.baud_candidates.iter().any(|&baud_rate| baud_rate == 0) {
            return Err(SerialReconnectError::InvalidConfig {
                field: "baud_candidates",
                reason: "must only contain baud rates greater than zero",
            });
        }
        if self.read_buffer_size == 0 {
            return Err(SerialReconnectError::InvalidConfig {
                field: "read_buffer_size",
                reason: "must be greater than zero",
            });
        }
        if self.initial_backoff == Duration::ZERO {
            return Err(SerialReconnectError::InvalidConfig {
                field: "initial_backoff",
                reason: "must be greater than zero",
            });
        }
        if self.max_backoff < self.initial_backoff {
            return Err(SerialReconnectError::InvalidConfig {
                field: "max_backoff",
                reason: "must be greater than or equal to initial_backoff",
            });
        }
        Ok(())
    }

    fn default_baud_candidates_for(baud_rate: u32) -> Vec<u32> {
        let mut baud_candidates = Vec::with_capacity(DEFAULT_BAUD_CANDIDATES.len() + 1);
        baud_candidates.push(baud_rate);
        baud_candidates.extend(DEFAULT_BAUD_CANDIDATES);
        Self::normalize_baud_candidates(baud_candidates)
    }

    fn normalize_baud_candidates(baud_candidates: impl IntoIterator<Item = u32>) -> Vec<u32> {
        let mut normalized = Vec::new();
        for baud_rate in baud_candidates {
            if !normalized.contains(&baud_rate) {
                normalized.push(baud_rate);
            }
        }
        normalized
    }
}

/// Non-recoverable supervisor construction errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SerialReconnectError {
    /// Configuration failed validation.
    #[error("invalid serial reconnect config: {field}: {reason}")]
    InvalidConfig {
        /// Invalid field name.
        field: &'static str,
        /// Validation failure reason.
        reason: &'static str,
    },
}

/// Observable result of a single supervisor poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SerialReconnectEvent {
    /// A serial port was opened successfully.
    Connected {
        /// Port name from the config.
        port_name: String,
        /// Monotonic open attempt counter.
        attempt: u64,
    },
    /// Opening failed and the caller should wait before polling again.
    ReconnectScheduled {
        /// Port name from the config.
        port_name: String,
        /// Monotonic open attempt counter.
        attempt: u64,
        /// Backoff delay selected for this failure.
        delay: Duration,
        /// I/O error kind returned by the factory.
        kind: io::ErrorKind,
        /// Human-readable error message.
        message: String,
    },
    /// Bytes were read from the connected port.
    Data {
        /// Bytes read from the serial port.
        bytes: Vec<u8>,
    },
    /// No bytes were available before the read timeout.
    Idle,
    /// The active port disconnected and was dropped.
    Disconnected {
        /// Port name from the config.
        port_name: String,
        /// Backoff delay selected before the next reopen attempt.
        next_delay: Duration,
        /// I/O error kind when the disconnect came from a read error.
        kind: Option<io::ErrorKind>,
        /// Human-readable disconnect reason.
        message: String,
    },
}

/// Counters maintained by the reconnect supervisor.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SerialReconnectStats {
    /// Total factory open attempts.
    pub open_attempts: u64,
    /// Total successful factory opens.
    pub successful_opens: u64,
    /// Total non-timeout read errors.
    pub read_errors: u64,
    /// Total disconnect detections, including zero-length reads.
    pub disconnects: u64,
    /// Total payload bytes delivered in `Data` events.
    pub bytes_read: u64,
}

/// Serial hot-plug supervisor with reopen-on-read-error behavior.
pub struct SerialReconnectSupervisor<F: SerialPortFactory> {
    config: SerialReconnectConfig,
    factory: F,
    port: Option<F::Port>,
    current_backoff: Duration,
    stats: SerialReconnectStats,
}

impl<F: SerialPortFactory> SerialReconnectSupervisor<F> {
    /// Create a supervisor from a config and a serial port factory.
    pub fn new(config: SerialReconnectConfig, factory: F) -> Result<Self, SerialReconnectError> {
        config.validate()?;
        let current_backoff = config.initial_backoff;
        Ok(Self {
            config,
            factory,
            port: None,
            current_backoff,
            stats: SerialReconnectStats::default(),
        })
    }

    /// Poll the supervisor once.
    ///
    /// When no port is connected, this attempts one reopen. When a port is
    /// connected, this attempts one read. Non-timeout read errors drop the
    /// handle so the following poll reopens through the factory.
    pub fn poll_once(&mut self) -> SerialReconnectEvent {
        if self.port.is_none() {
            return self.open_once();
        }

        let mut buf = vec![0; self.config.read_buffer_size];
        let read = self.port.as_mut().expect("port checked above").read(&mut buf);

        match read {
            Ok(0) => self.disconnect(None, "zero-length read"),
            Ok(n) => {
                buf.truncate(n);
                self.stats.bytes_read += n as u64;
                SerialReconnectEvent::Data { bytes: buf }
            }
            Err(err) if Self::is_idle_error(err.kind()) => SerialReconnectEvent::Idle,
            Err(err) => {
                let kind = err.kind();
                let message = err.to_string();
                self.stats.read_errors += 1;
                self.disconnect(Some(kind), message)
            }
        }
    }

    /// Current configuration.
    pub fn config(&self) -> &SerialReconnectConfig {
        &self.config
    }

    /// Factory reference, useful for inspecting fake factories in tests.
    pub fn factory(&self) -> &F {
        &self.factory
    }

    /// Mutable factory reference.
    pub fn factory_mut(&mut self) -> &mut F {
        &mut self.factory
    }

    /// Snapshot of supervisor counters.
    pub fn stats(&self) -> SerialReconnectStats {
        self.stats
    }

    /// Whether a serial handle is currently connected.
    pub fn is_connected(&self) -> bool {
        self.port.is_some()
    }

    /// Active baud rate from the most recent successful probe, or the next configured default.
    pub fn current_baud_rate(&self) -> u32 {
        self.config.baud_rate
    }

    /// Ordered baud rates probed by this supervisor.
    pub fn baud_candidates(&self) -> &[u32] {
        self.config.baud_candidates()
    }

    /// Delay that will be reported for the next reconnect failure.
    pub fn next_reconnect_delay(&self) -> Duration {
        self.current_backoff
    }

    fn open_once(&mut self) -> SerialReconnectEvent {
        let mut last_error = None;
        for baud_rate in self.config.baud_candidates.clone() {
            self.stats.open_attempts += 1;
            let attempt = self.stats.open_attempts;
            let mut probe_config = self.config.clone();
            probe_config.baud_rate = baud_rate;

            match self.factory.open(&probe_config) {
                Ok(port) => {
                    self.config.baud_rate = baud_rate;
                    self.port = Some(port);
                    self.stats.successful_opens += 1;
                    self.current_backoff = self.config.initial_backoff;
                    return SerialReconnectEvent::Connected {
                        port_name: self.config.port_name.clone(),
                        attempt,
                    };
                }
                Err(err) => {
                    last_error = Some((attempt, err.kind(), err.to_string()));
                }
            }
        }

        let (attempt, kind, message) =
            last_error.expect("baud_candidates is validated as non-empty");
        let delay = self.consume_backoff();
        SerialReconnectEvent::ReconnectScheduled {
            port_name: self.config.port_name.clone(),
            attempt,
            delay,
            kind,
            message,
        }
    }

    fn disconnect(
        &mut self,
        kind: Option<io::ErrorKind>,
        message: impl Into<String>,
    ) -> SerialReconnectEvent {
        self.port = None;
        self.stats.disconnects += 1;
        let next_delay = self.consume_backoff();
        SerialReconnectEvent::Disconnected {
            port_name: self.config.port_name.clone(),
            next_delay,
            kind,
            message: message.into(),
        }
    }

    fn consume_backoff(&mut self) -> Duration {
        let delay = self.current_backoff;
        self.current_backoff = cmp::min(
            self.current_backoff
                .checked_mul(2)
                .unwrap_or(self.config.max_backoff),
            self.config.max_backoff,
        );
        delay
    }

    fn is_idle_error(kind: io::ErrorKind) -> bool {
        matches!(
            kind,
            io::ErrorKind::Interrupted | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        )
    }
}

#[cfg(any(test, feature = "serial-reconnect-testkit"))]
#[path = "serial_reconnect_testkit.rs"]
mod testkit;

#[cfg(any(test, feature = "serial-reconnect-testkit"))]
pub use testkit::{FakeOpenAction, FakeReadAction, FakeSerialPort, FakeSerialPortFactory};

#[cfg(test)]
#[path = "serial_reconnect_tests.rs"]
mod tests;
