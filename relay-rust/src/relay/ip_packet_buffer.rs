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
use super::ip_packet::{
    IpPacket, MAX_IP_PACKET_LENGTH, MAX_IPV4_PACKET_LENGTH, MAX_IPV6_PACKET_LENGTH,
    peek_version_length,
};
use super::ipv4_header::IPV4_MIN_HEADER_LENGTH;
use super::ipv6_header::IPV6_HEADER_LENGTH;

use crate::{trace, warn};
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

    /// Return the length of the valid packet at the head of the buffer, if
    /// complete.
    ///
    /// Invalid or unknown bytes are dropped so that a corrupted stream cannot
    /// block all subsequent packets.
    fn available_packet_length(&mut self) -> Option<usize> {
        loop {
            let data = self.buf.peek();
            trace!(target: TAG, "Parse packet: {}", binary::build_packet_string(data));
            let (version, length) = peek_version_length(data)?;
            let (min_length, max_length) = match version {
                4 => (IPV4_MIN_HEADER_LENGTH, MAX_IPV4_PACKET_LENGTH),
                6 => (IPV6_HEADER_LENGTH, MAX_IPV6_PACKET_LENGTH),
                _ => {
                    warn!(target: TAG, "Dropping unknown IP version={}", version);
                    self.buf.consume(1);
                    continue;
                }
            };
            if length < min_length || length > max_length {
                warn!(
                    target: TAG,
                    "Dropping malformed IPv{} packet: length={}",
                    version,
                    length
                );
                self.buf.consume(1);
                continue;
            }
            if length > data.len() {
                // no full packet available
                return None;
            }
            let valid = {
                let data = self.buf.peek_mut();
                IpPacket::parse(&mut data[..length]).is_some()
            };
            if valid {
                return Some(length);
            }
            warn!(
                target: TAG,
                "Dropping malformed IP packet: {}",
                binary::build_packet_string(&self.buf.peek()[..length])
            );
            self.buf.consume(1);
        }
    }

    pub fn as_ip_packet(&mut self) -> Option<IpPacket<'_>> {
        let length = self.available_packet_length()?;
        let data = self.buf.peek_mut();
        IpPacket::parse(&mut data[..length])
    }

    pub fn next(&mut self) {
        // remove the packet in front of the buffer
        let length = self
            .available_packet_length()
            .expect("next() called while there was no packet");
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

    fn read_into_buffer(raw: &[u8]) -> IpPacketBuffer {
        let mut buf = IpPacketBuffer::new();
        let mut cursor = io::Cursor::new(raw);
        buf.read_from(&mut cursor).unwrap();
        buf
    }

    #[test]
    fn parse_mixed_packets() {
        let mut raw = Vec::new();
        write_ipv4_packet_to(&mut raw);
        write_ipv6_packet_to(&mut raw);
        let mut buf = read_into_buffer(&raw);

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

    #[test]
    fn parse_maximum_ipv6_packet() {
        let mut raw = Vec::with_capacity(MAX_IPV6_PACKET_LENGTH);
        raw.push(6u8 << 4);
        raw.push(0);
        raw.extend_from_slice(&(0u16).to_be_bytes());
        let payload_length = u16::MAX; // 8-byte UDP header + 65527-byte payload
        raw.extend_from_slice(&payload_length.to_be_bytes());
        raw.push(17);
        raw.push(64);
        raw.extend_from_slice(&[0xfd; 16]);
        raw.extend_from_slice(&[0x20; 16]);
        raw.extend_from_slice(&(1234u16).to_be_bytes());
        raw.extend_from_slice(&(5678u16).to_be_bytes());
        raw.extend_from_slice(&payload_length.to_be_bytes());
        raw.extend_from_slice(&(0u16).to_be_bytes());
        raw.resize(MAX_IPV6_PACKET_LENGTH, 0x42);
        assert_eq!(MAX_IPV6_PACKET_LENGTH, raw.len());

        let mut buf = read_into_buffer(&raw);
        let packet = buf.as_ip_packet().unwrap();
        assert_eq!(MAX_IPV6_PACKET_LENGTH, packet.length());
        assert_eq!(
            usize::from(payload_length) - usize::from(super::super::udp_header::UDP_HEADER_LENGTH),
            packet.payload().unwrap().len()
        );
        buf.next();
        assert!(buf.as_ip_packet().is_none());
    }

    #[test]
    fn resync_after_garbage() {
        let mut raw = vec![0x00, 0x99, 0x42];
        write_ipv4_packet_to(&mut raw);
        write_ipv6_packet_to(&mut raw);
        let mut buf = read_into_buffer(&raw);

        let p1 = buf.as_ip_packet().unwrap();
        assert_eq!(4, p1.version());
        buf.next();
        let p2 = buf.as_ip_packet().unwrap();
        assert_eq!(6, p2.version());
    }

    #[test]
    fn drop_malformed_ipv4_header() {
        // an IPv4 header whose total length is smaller than the minimal header
        let mut raw = Vec::new();
        raw.push(4u8 << 4 | 5);
        raw.push(0);
        raw.extend_from_slice(&(10u16).to_be_bytes());
        raw.extend_from_slice(&[0u8; 16]);
        write_ipv4_packet_to(&mut raw);
        write_ipv6_packet_to(&mut raw);

        let mut buf = read_into_buffer(&raw);
        let p1 = buf.as_ip_packet().unwrap();
        assert_eq!(4, p1.version());
        assert_eq!(32, p1.length());
        buf.next();
        let p2 = buf.as_ip_packet().unwrap();
        assert_eq!(6, p2.version());
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        let mut state = 0x89ab_cdefu32;
        for length in 0..256 {
            let mut raw = vec![0u8; length];
            for byte in &mut raw {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *byte = (state >> 24) as u8;
            }
            let mut buf = read_into_buffer(&raw);
            let _ = buf.as_ip_packet();
        }
    }
}
