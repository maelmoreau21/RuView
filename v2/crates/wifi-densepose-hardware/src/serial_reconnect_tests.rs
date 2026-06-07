use std::io;
use std::time::Duration;

use super::{
    FakeSerialPort, FakeSerialPortFactory, SerialReconnectConfig, SerialReconnectEvent,
    SerialReconnectError, SerialReconnectSupervisor, DEFAULT_BAUD_CANDIDATES,
};

fn test_config() -> SerialReconnectConfig {
    let mut config = SerialReconnectConfig::new("COM12", 115_200);
    config.initial_backoff = Duration::from_millis(10);
    config.max_backoff = Duration::from_millis(40);
    config.read_buffer_size = 8;
    config
}

#[test]
fn open_failures_use_exponential_backoff_until_cap() {
    let mut factory = FakeSerialPortFactory::new();
    factory.push_open_error(io::ErrorKind::NotFound, "missing 1");
    factory.push_open_error(io::ErrorKind::NotFound, "missing 2");
    factory.push_open_error(io::ErrorKind::NotFound, "missing 3");

    let mut supervisor =
        SerialReconnectSupervisor::new(test_config(), factory).expect("valid config");

    for expected in [10, 20, 40] {
        match supervisor.poll_once() {
            SerialReconnectEvent::ReconnectScheduled { delay, .. } => {
                assert_eq!(delay, Duration::from_millis(expected));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    assert_eq!(supervisor.next_reconnect_delay(), Duration::from_millis(40));
    assert_eq!(supervisor.stats().open_attempts, 9);
    let expected_baud_attempts = DEFAULT_BAUD_CANDIDATES.as_slice().repeat(3);
    assert_eq!(
        supervisor.factory().baud_attempts(),
        expected_baud_attempts.as_slice()
    );
}

#[test]
fn open_probes_default_baud_candidates_until_one_connects() {
    let mut port = FakeSerialPort::new();
    port.push_bytes(b"ok");

    let mut factory = FakeSerialPortFactory::new();
    factory.push_open_error(io::ErrorKind::InvalidInput, "unsupported baud");
    factory.push_port(port);

    let mut supervisor =
        SerialReconnectSupervisor::new(test_config(), factory).expect("valid config");

    assert_eq!(
        supervisor.config().baud_candidates,
        DEFAULT_BAUD_CANDIDATES.to_vec()
    );
    assert!(matches!(
        supervisor.poll_once(),
        SerialReconnectEvent::Connected { attempt: 2, .. }
    ));
    assert_eq!(supervisor.config().baud_rate, 460_800);
    assert_eq!(supervisor.factory().baud_attempts(), &[115_200, 460_800]);
    assert_eq!(supervisor.next_reconnect_delay(), Duration::from_millis(10));
    assert_eq!(
        supervisor.poll_once(),
        SerialReconnectEvent::Data {
            bytes: b"ok".to_vec()
        }
    );
}

#[test]
fn configured_default_baud_is_probed_first_even_when_in_default_list() {
    let config = SerialReconnectConfig::new("COM12", 460_800);
    assert_eq!(config.baud_candidates(), &[460_800, 115_200, 921_600]);

    let supervisor =
        SerialReconnectSupervisor::new(config, FakeSerialPortFactory::new()).expect("valid config");
    assert_eq!(supervisor.current_baud_rate(), 460_800);
    assert_eq!(supervisor.baud_candidates(), &[460_800, 115_200, 921_600]);
}

#[test]
fn read_error_disconnects_and_next_poll_reopens() {
    let mut first = FakeSerialPort::new();
    first.push_bytes(b"abc");
    first.push_error(io::ErrorKind::BrokenPipe, "usb unplugged");

    let mut second = FakeSerialPort::new();
    second.push_bytes(b"def");

    let mut factory = FakeSerialPortFactory::new();
    factory.push_port(first);
    factory.push_port(second);

    let mut supervisor =
        SerialReconnectSupervisor::new(test_config(), factory).expect("valid config");

    assert!(matches!(
        supervisor.poll_once(),
        SerialReconnectEvent::Connected { attempt: 1, .. }
    ));
    assert_eq!(
        supervisor.poll_once(),
        SerialReconnectEvent::Data {
            bytes: b"abc".to_vec()
        }
    );
    assert_eq!(
        supervisor.poll_once(),
        SerialReconnectEvent::Disconnected {
            port_name: "COM12".to_string(),
            next_delay: Duration::from_millis(10),
            kind: Some(io::ErrorKind::BrokenPipe),
            message: "usb unplugged".to_string(),
        }
    );
    assert!(!supervisor.is_connected());
    assert!(matches!(
        supervisor.poll_once(),
        SerialReconnectEvent::Connected { attempt: 2, .. }
    ));
    assert_eq!(
        supervisor.poll_once(),
        SerialReconnectEvent::Data {
            bytes: b"def".to_vec()
        }
    );

    let stats = supervisor.stats();
    assert_eq!(stats.open_attempts, 2);
    assert_eq!(stats.successful_opens, 2);
    assert_eq!(stats.read_errors, 1);
    assert_eq!(stats.disconnects, 1);
    assert_eq!(stats.bytes_read, 6);
    assert_eq!(supervisor.factory().open_attempts(), 2);
}

#[test]
fn zero_length_read_is_a_disconnect_signal() {
    let mut port = FakeSerialPort::new();
    port.push_empty();
    let mut factory = FakeSerialPortFactory::new();
    factory.push_port(port);
    let mut supervisor =
        SerialReconnectSupervisor::new(test_config(), factory).expect("valid config");

    assert!(matches!(
        supervisor.poll_once(),
        SerialReconnectEvent::Connected { .. }
    ));
    assert_eq!(
        supervisor.poll_once(),
        SerialReconnectEvent::Disconnected {
            port_name: "COM12".to_string(),
            next_delay: Duration::from_millis(10),
            kind: None,
            message: "zero-length read".to_string(),
        }
    );

    assert_eq!(supervisor.stats().disconnects, 1);
    assert_eq!(supervisor.stats().read_errors, 0);
}

#[test]
fn read_timeout_is_idle_and_keeps_port_open() {
    let mut port = FakeSerialPort::new();
    port.push_error(io::ErrorKind::TimedOut, "quiet");
    port.push_bytes(b"ok");
    let mut factory = FakeSerialPortFactory::new();
    factory.push_port(port);
    let mut supervisor =
        SerialReconnectSupervisor::new(test_config(), factory).expect("valid config");

    assert!(matches!(
        supervisor.poll_once(),
        SerialReconnectEvent::Connected { .. }
    ));
    assert_eq!(supervisor.poll_once(), SerialReconnectEvent::Idle);
    assert!(supervisor.is_connected());
    assert_eq!(
        supervisor.poll_once(),
        SerialReconnectEvent::Data {
            bytes: b"ok".to_vec()
        }
    );
    assert_eq!(supervisor.stats().open_attempts, 1);
}

#[test]
fn invalid_port_names_are_rejected_before_events_can_expose_them() {
    let cases = ["", "COM12\nbad", "COM12\0bad"];

    for name in cases {
        let err = match SerialReconnectSupervisor::new(
            SerialReconnectConfig::new(name, 115_200),
            FakeSerialPortFactory::new(),
        ) {
            Err(err) => err,
            Ok(_) => panic!("invalid port name must fail"),
        };

        assert!(matches!(
            err,
            SerialReconnectError::InvalidConfig {
                field: "port_name",
                ..
            }
        ));
    }

    let long_name = "A".repeat(129);
    let err = match SerialReconnectSupervisor::new(
        SerialReconnectConfig::new(long_name, 115_200),
        FakeSerialPortFactory::new(),
    ) {
        Err(err) => err,
        Ok(_) => panic!("oversized port name must fail"),
    };

    assert!(matches!(
        err,
        SerialReconnectError::InvalidConfig {
            field: "port_name",
            ..
        }
    ));
}

#[test]
fn invalid_baud_candidates_are_rejected() {
    for candidates in [Vec::new(), vec![115_200, 0]] {
        let err = match SerialReconnectSupervisor::new(
            test_config().with_baud_candidates(candidates),
            FakeSerialPortFactory::new(),
        ) {
            Err(err) => err,
            Ok(_) => panic!("invalid baud candidates must fail"),
        };

        assert!(matches!(
            err,
            SerialReconnectError::InvalidConfig {
                field: "baud_candidates",
                ..
            }
        ));
    }
}
