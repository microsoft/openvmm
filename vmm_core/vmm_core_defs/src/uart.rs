// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UART identities selected for device-tree publication.

use mesh::payload::Protobuf;
use serial_16550_resources::ComPort;

/// A UART at its standard platform location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Protobuf)]
pub enum UartId {
    /// A PC COM port.
    Com(ComPort),
    /// The first ARM64 PL011 UART.
    Pl0110,
    /// The second ARM64 PL011 UART.
    Pl0111,
}

/// Required UART inventory for configuration transport.
///
/// The single variant makes an absent field a decode error. A bare mesh `Vec`
/// would silently decode an absent inventory as an empty list. `Devices([])`
/// explicitly requests no UART nodes.
#[derive(Debug, Protobuf)]
pub enum UartInventory {
    /// UARTs selected for publication, independent of console selection.
    Devices(Vec<UartId>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[derive(Protobuf)]
    struct Config {
        uarts: UartInventory,
        console: Option<UartId>,
    }

    #[test]
    fn inventory_round_trip() {
        for devices in [
            vec![],
            vec![
                UartId::Com(ComPort::Com1),
                UartId::Com(ComPort::Com2),
                UartId::Com(ComPort::Com3),
                UartId::Com(ComPort::Com4),
            ],
            vec![UartId::Pl0110, UartId::Pl0111],
        ] {
            let config = Config {
                uarts: UartInventory::Devices(devices.clone()),
                console: None,
            };
            let encoded = mesh::payload::encode(config);
            let decoded: Config = mesh::payload::decode(&encoded).unwrap();
            let UartInventory::Devices(actual) = decoded.uarts;
            assert_eq!(actual, devices);
            assert_eq!(decoded.console, None);
        }
    }

    #[test]
    fn console_selection_round_trip() {
        let console = UartId::Com(ComPort::Com3);
        let encoded = mesh::payload::encode(Config {
            uarts: UartInventory::Devices(vec![console]),
            console: Some(console),
        });
        let decoded: Config = mesh::payload::decode(&encoded).unwrap();
        assert_eq!(decoded.console, Some(console));
    }

    #[test]
    fn missing_inventory_is_rejected() {
        assert!(mesh::payload::decode::<Config>(&[]).is_err());
        assert!(mesh::payload::decode::<Config>(&[0x0a, 0]).is_err());
    }
}
