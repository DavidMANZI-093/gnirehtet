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

use super::binary;
use super::byte_buffer::ByteBuffer;
use super::ip_packet::{IpPacket, MAX_IP_PACKET_LENGTH, peek_version_length};
use super::ipv4_packet::Ipv4Packet;
use super::ipv6_packet::Ipv6Packet;

use crate::trace;
use std::io;

const TAG: &str = "IpPacketBuffer";

pub struct IpPacketBuffer {
    buf: ByteBuffer,
}

impl IpPacketBuffer {
    pub fn new() -> Self {
        Self {
            buf: ByteBuffer::new(MAX_IP_PACKET_LENGTH),
        }
    }

    pub fn read_from<R: io::Read>(&mut self, source: &mut R) -> io::Result<bool> {
        self.buf.read_from(source)
    }

    pub fn remaining(&self) -> usize {
        self.buf.remaining()
    }

    fn available_packet_length(&self) -> Option<u16> {
        let data = self.buf.peek();
        trace!(target: TAG, "Parse packet: {}", binary::build_packet_string(data));
        if let Some((version, length)) = peek_version_length(data) {
            assert!(
                version == 4 || version == 6,
                "Unexpected IP version={}",
                version
            );
            if length as usize <= data.len() {
                // full packet available
                Some(length)
            } else {
                // no full packet available
                None
            }
        } else {
            // no packet
            None
        }
    }

    pub fn as_ip_packet(&mut self) -> Option<IpPacket<'_>> {
        if self.available_packet_length().is_some() {
            let data = self.buf.peek_mut();
            match data[0] >> 4 {
                4 => Some(IpPacket::V4(Ipv4Packet::parse(data))),
                6 => Some(IpPacket::V6(Ipv6Packet::parse(data))),
                _ => None,
            }
        } else {
            None
        }
    }

    pub fn next(&mut self) {
        // remove the packet in front of the buffer
        let length = self
            .available_packet_length()
            .expect("next() called while there was no packet") as usize;
        self.buf.consume(length);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn write_ipv4_packet_to(raw: &mut Vec<u8>) {
        raw.push(4u8 << 4 | 5);
        raw.push(0);
        raw.extend_from_slice(&(32u16).to_be_bytes());
        raw.extend_from_slice(&(0u32).to_be_bytes());
        raw.push(0);
        raw.push(17);
        raw.extend_from_slice(&(0u16).to_be_bytes());
        raw.extend_from_slice(&(0x12345678u32).to_be_bytes());
        raw.extend_from_slice(&(0x42424242u32).to_be_bytes());
        raw.extend_from_slice(&(1234u16).to_be_bytes());
        raw.extend_from_slice(&(5678u16).to_be_bytes());
        raw.extend_from_slice(&(12u16).to_be_bytes());
        raw.extend_from_slice(&(0u16).to_be_bytes());
        raw.extend_from_slice(&(0x11223344u32).to_be_bytes());
    }

    fn write_ipv6_packet_to(raw: &mut Vec<u8>) {
        raw.push(6u8 << 4);
        raw.push(0);
        raw.extend_from_slice(&(0u16).to_be_bytes());
        raw.extend_from_slice(&(12u16).to_be_bytes());
        raw.push(17);
        raw.push(64);
        raw.extend_from_slice(&[0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        raw.extend_from_slice(&[0x20, 1, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        raw.extend_from_slice(&(1234u16).to_be_bytes());
        raw.extend_from_slice(&(5678u16).to_be_bytes());
        raw.extend_from_slice(&(12u16).to_be_bytes());
        raw.extend_from_slice(&(0u16).to_be_bytes());
        raw.extend_from_slice(&(0x11223344u32).to_be_bytes());
    }

    #[test]
    fn parse_mixed_packets() {
        let mut raw = Vec::new();
        write_ipv4_packet_to(&mut raw);
        write_ipv6_packet_to(&mut raw);
        let mut buf = IpPacketBuffer::new();
        let mut cursor = io::Cursor::new(raw);
        buf.read_from(&mut cursor).unwrap();

        let p1 = buf.as_ip_packet().unwrap();
        assert_eq!(4, p1.version());
        assert_eq!(32, p1.length());
        buf.next();
        let p2 = buf.as_ip_packet().unwrap();
        assert_eq!(6, p2.version());
        assert_eq!(52, p2.length());
        buf.next();
        assert!(buf.as_ip_packet().is_none());
    }

    #[test]
    fn parse_fragmented_ipv6_packet() {
        let mut raw = Vec::new();
        write_ipv6_packet_to(&mut raw);
        let mut buf = IpPacketBuffer::new();
        let mut cursor = io::Cursor::new(&raw[..20]);
        buf.read_from(&mut cursor).unwrap();
        assert!(buf.as_ip_packet().is_none());
        let mut cursor = io::Cursor::new(&raw[20..]);
        buf.read_from(&mut cursor).unwrap();
        let p = buf.as_ip_packet().unwrap();
        assert_eq!(6, p.version());
    }
}
