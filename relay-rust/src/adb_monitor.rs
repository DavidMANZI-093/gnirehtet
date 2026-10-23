/*
 * Copyright (C) 2017 Genymobile
 * Copyright (C) 2026 David MANZI
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::relay::byte_buffer::ByteBuffer;
use crate::{error, info};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::process;
use std::str;
use std::thread;
use std::time::Duration;

const TAG: &str = "AdbMonitor";

#[inline]
pub(crate) fn get_adb_path() -> String {
    if let Some(env_adb) = std::env::var_os("ADB") {
        env_adb.into_string().expect("invalid ADB value")
    } else {
        "adb".to_string()
    }
}

pub struct AdbMonitor {
    callback: Box<dyn Fn(&str)>,
    buf: ByteBuffer,
    connected_devices: Vec<String>,
}

impl AdbMonitor {
    const TRACK_DEVICES_REQUEST: &[u8] = b"0012host:track-devices";
    const BUFFER_SIZE: usize = 1024;
    const RETRY_DELAY_ADB_DAEMON_OK: Duration = Duration::from_secs(1);
    const RETRY_DELAY_ADB_DAEMON_KO: Duration = Duration::from_secs(5);

    pub fn new(callback: Box<dyn Fn(&str)>) -> Self {
        Self {
            callback,
            buf: ByteBuffer::new(Self::BUFFER_SIZE),
            connected_devices: Vec::new(),
        }
    }

    pub fn monitor(&mut self) {
        loop {
            if let Err(err) = self.track_devices() {
                error!(target: TAG, "Failed to monitor adb devices: {}", err);
                Self::repair_adb_daemon();
            }
        }
    }

    fn track_devices(&mut self) -> io::Result<()> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, 5037));
        let mut stream = TcpStream::connect(address)?;
        self.track_devices_on_stream(&mut stream)
    }

    fn track_devices_on_stream(&mut self, stream: &mut TcpStream) -> io::Result<()> {
        stream.write_all(Self::TRACK_DEVICES_REQUEST)?;
        if self.consume_okay(stream)? {
            loop {
                let packet = self.next_packet(stream)?;
                self.handle_packet(packet.as_str());
            }
        }
        Ok(())
    }

    fn consume_okay(&mut self, stream: &mut TcpStream) -> io::Result<bool> {
        while self.buf.peek().len() < 4 {
            if !self.buf.read_from(stream)? {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ADB daemon closed the 'track-devices' connection",
                ));
            }
        }
        let ok = b"OKAY" == &self.buf.peek()[0..4];
        self.buf.consume(4);
        Ok(ok)
    }

    fn read_packet(buf: &mut ByteBuffer) -> io::Result<Option<String>> {
        let packet_length = Self::available_packet_length(buf.peek())?;
        if let Some(len) = packet_length {
            // retrieve the content and consume the packet
            let data = Self::binary_to_string(&buf.peek()[4..len])?;
            buf.consume(len);
            Ok(Some(data))
        } else {
            Ok(None)
        }
    }

    fn next_packet(&mut self, stream: &mut TcpStream) -> io::Result<String> {
        loop {
            if let Some(packet) = Self::read_packet(&mut self.buf)? {
                return Ok(packet);
            }
            self.fill_buffer_from(stream)?;
        }
    }

    fn fill_buffer_from(&mut self, stream: &mut TcpStream) -> io::Result<()> {
        if self.buf.read_from(stream)? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "ADB daemon closed the 'track-devices' connection",
            ))
        }
    }

    fn available_packet_length(input: &[u8]) -> io::Result<Option<usize>> {
        if input.len() < 4 {
            Ok(None)
        } else {
            // each packet contains 4 bytes representing the String length in hexa, followed by a
            // list of device information;
            // each line contains: the device serial, `\t', the state, '\n'
            // for example:
            // "00360123456789abcdef\tdevice\nfedcba9876543210\tunauthorized\n":
            //  - 0036 indicates that the data is 0x36 (54) bytes length
            //  - the device with serial 0123456789abcdef is connected
            //  - the device with serial fedcba9876543210 is unauthorized
            let len = Self::parse_length(&input[0..4])?;
            if len > Self::BUFFER_SIZE as u32 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Packet size should not be that big: {}", len),
                ));
            }
            if input.len() - 4 >= len as usize {
                Ok(Some(4 + len as usize))
            } else {
                // not enough data
                Ok(None)
            }
        }
    }

    fn handle_packet(&mut self, packet: &str) {
        let current_connected_devices = self.parse_connected_devices(packet);
        for serial in &current_connected_devices {
            if !self.connected_devices.contains(serial) {
                (self.callback)(serial);
            }
        }
        self.connected_devices = current_connected_devices;
    }

    fn parse_connected_devices(&self, packet: &str) -> Vec<String> {
        packet
            .lines()
            .filter_map(|line| {
                let mut split = line.split_whitespace();
                let serial = split.next()?;
                let state = split.next()?;
                if state == "device" {
                    Some(serial.to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    fn parse_length(data: &[u8]) -> io::Result<u32> {
        assert_eq!(data.len(), 4, "Invalid length field value");
        let hexa = str::from_utf8(data).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Cannot read hexa length as UTF-8 ({})", err),
            )
        })?;
        u32::from_str_radix(hexa, 16).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Cannot parse hexa length ({})", err),
            )
        })
    }

    fn repair_adb_daemon() {
        let delay = if Self::start_adb_daemon() {
            Self::RETRY_DELAY_ADB_DAEMON_OK
        } else {
            Self::RETRY_DELAY_ADB_DAEMON_KO
        };
        thread::sleep(delay);
    }

    fn start_adb_daemon() -> bool {
        info!(target: TAG, "Restarting adb daemon");
        match process::Command::new(get_adb_path())
            .arg("start-server")
            .status()
        {
            Ok(exit_status) if exit_status.success() => true,
            Ok(_) => {
                error!(
                    target: TAG,
                    "Could not restart adb daemon (exited on error)"
                );
                false
            }
            Err(err) => {
                error!(target: TAG, "Could not restart adb daemon: {}", err);
                false
            }
        }
    }

    fn binary_to_string(data: &[u8]) -> io::Result<String> {
        str::from_utf8(data).map(str::to_owned).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Track-devices string is not valid UTF-8",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn test_read_valid_packet() {
        let mut buf = ByteBuffer::new(64);
        let raw = "00180123456789ABCDEF\tdevice\n".as_bytes();

        let mut cursor = io::Cursor::new(raw);
        buf.read_from(&mut cursor).unwrap();

        let packet = AdbMonitor::read_packet(&mut buf).unwrap().unwrap();
        assert_eq!("0123456789ABCDEF\tdevice\n", packet);
    }

    #[test]
    fn test_read_valid_packets() {
        let mut buf = ByteBuffer::new(64);
        let raw = "00300123456789ABCDEF\tdevice\nFEDCBA9876543210\tdevice\n".as_bytes();

        let mut cursor = io::Cursor::new(raw);
        buf.read_from(&mut cursor).unwrap();

        let packet = AdbMonitor::read_packet(&mut buf).unwrap().unwrap();
        assert_eq!(
            "0123456789ABCDEF\tdevice\nFEDCBA9876543210\tdevice\n",
            packet
        );
    }

    #[test]
    fn test_read_valid_packet_with_garbage() {
        let mut buf = ByteBuffer::new(64);
        let raw = "00180123456789ABCDEF\tdevice\ngarbage".as_bytes();

        let mut cursor = io::Cursor::new(raw);
        buf.read_from(&mut cursor).unwrap();

        let packet = AdbMonitor::read_packet(&mut buf).unwrap().unwrap();
        assert_eq!("0123456789ABCDEF\tdevice\n", packet);
    }

    #[test]
    fn test_read_short_packet() {
        let mut buf = ByteBuffer::new(64);
        let raw = "00180123456789ABCDEF\tdevi".as_bytes();

        let mut cursor = io::Cursor::new(raw);
        buf.read_from(&mut cursor).unwrap();

        let packet = AdbMonitor::read_packet(&mut buf).unwrap();
        assert!(packet.is_none());
    }

    #[test]
    fn test_handle_packet_device() {
        let serial = Rc::new(RefCell::new(None));
        let serial_clone = serial.clone();

        let mut monitor = AdbMonitor::new(Box::new(move |serial: &str| {
            serial_clone.replace(Some(serial.to_string()));
        }));
        monitor.handle_packet("0123456789ABCDEF\tdevice\n");

        assert_eq!("0123456789ABCDEF", serial.borrow().as_ref().unwrap());
    }

    #[test]
    fn test_handle_packet_offline() {
        let serial = Rc::new(RefCell::new(None));
        let serial_clone = serial.clone();

        let mut monitor = AdbMonitor::new(Box::new(move |serial: &str| {
            serial_clone.replace(Some(serial.to_string()));
        }));
        monitor.handle_packet("0123456789ABCDEF\toffline\n");

        assert!(serial.borrow().is_none());
    }

    #[test]
    fn test_multiple_connected_devices() {
        let serials = Rc::new(RefCell::new(Vec::new()));
        let serials_clone = serials.clone();

        let mut monitor = AdbMonitor::new(Box::new(move |serial: &str| {
            serials_clone.borrow_mut().push(serial.to_string());
        }));
        monitor.handle_packet("0123456789ABCDEF\tdevice\nFEDCBA9876543210\tdevice\n");

        let vec = serials.borrow();
        assert_eq!(2, vec.len());
        assert_eq!("0123456789ABCDEF", vec[0]);
        assert_eq!("FEDCBA9876543210", vec[1]);
    }

    #[test]
    fn test_multiple_connected_devices_with_disconnection() {
        let serials = Rc::new(RefCell::new(Vec::new()));
        let serials_clone = serials.clone();

        let mut monitor = AdbMonitor::new(Box::new(move |serial: &str| {
            serials_clone.borrow_mut().push(serial.to_string());
        }));
        monitor.handle_packet("0123456789ABCDEF\tdevice\nFEDCBA9876543210\tdevice\n");
        monitor.handle_packet("0123456789ABCDEF\tdevice\n");
        monitor.handle_packet("0123456789ABCDEF\tdevice\nFEDCBA9876543210\tdevice\n");

        let vec = serials.borrow();
        assert_eq!(3, vec.len());
        assert_eq!("0123456789ABCDEF", vec[0]);
        assert_eq!("FEDCBA9876543210", vec[1]);
        assert_eq!("FEDCBA9876543210", vec[2]);
    }
}
