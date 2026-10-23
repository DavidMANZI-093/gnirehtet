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
use super::ipv4_header;
use super::ipv4_packet::{Ipv4Packet, MAX_PACKET_LENGTH};

use crate::trace;
use std::io;

const TAG: &str = "Ipv4PacketBuffer";

pub struct Ipv4PacketBuffer {
    buf: ByteBuffer,
}

impl Ipv4PacketBuffer {
    pub fn new() -> Self {
        Self {
            buf: ByteBuffer::new(MAX_PACKET_LENGTH),
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
        if let Some((version, length)) = ipv4_header::peek_version_length(data) {
            assert!(version == 4, "Not an Ipv4 packet, version={}", version);
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

    pub fn as_ipv4_packet(&mut self) -> Option<Ipv4Packet<'_>> {
        if self.available_packet_length().is_some() {
            let data = self.buf.peek_mut();
            Some(Ipv4Packet::parse(data))
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
    use crate::relay::ipv4_header::Protocol;
    use crate::relay::transport_header::TransportHeaderData;
    use std::io;

    fn create_packet() -> Vec<u8> {
        let mut raw = Vec::new();
        write_packet_to(&mut raw);
        raw
    }

    fn write_packet_to(raw: &mut Vec<u8>) {
        raw.push(4u8 << 4 | 5);
        raw.push(0); // ToS
        raw.extend_from_slice(&(32u16).to_be_bytes()); // total length 20 + 8 + 4
        raw.extend_from_slice(&(0u32).to_be_bytes()); // id_flags_fragment_offset
        raw.push(0); // TTL
        raw.push(17); // protocol (UDP)
        raw.extend_from_slice(&(0u16).to_be_bytes()); // checksum
        raw.extend_from_slice(&(0x12345678u32).to_be_bytes()); // source address
        raw.extend_from_slice(&(0x42424242u32).to_be_bytes()); // destination address

        raw.extend_from_slice(&(1234u16).to_be_bytes()); // source port
        raw.extend_from_slice(&(5678u16).to_be_bytes()); // destination port
        raw.extend_from_slice(&(12u16).to_be_bytes()); // length
        raw.extend_from_slice(&(0u16).to_be_bytes()); // checksum

        raw.extend_from_slice(&(0x11223344u32).to_be_bytes()); // payload
    }

    fn write_another_packet_to(raw: &mut Vec<u8>) {
        raw.push(4u8 << 4 | 5);
        raw.push(0); // ToS
        raw.extend_from_slice(&(29u16).to_be_bytes()); // total length 20 + 8 + 1
        raw.extend_from_slice(&(0u32).to_be_bytes()); // id_flags_fragment_offset
        raw.push(0); // TTL
        raw.push(17); // protocol (UDP)
        raw.extend_from_slice(&(0u16).to_be_bytes()); // checksum
        raw.extend_from_slice(&(0x11111111u32).to_be_bytes()); // source address
        raw.extend_from_slice(&(0x22222222u32).to_be_bytes()); // destination address

        raw.extend_from_slice(&(1111u16).to_be_bytes()); // source port
        raw.extend_from_slice(&(2222u16).to_be_bytes()); // destination port
        raw.extend_from_slice(&(9u16).to_be_bytes()); // length
        raw.extend_from_slice(&(0u16).to_be_bytes()); // checksum

        raw.push(0x99); // payload
    }

    fn check_packet_headers(ipv4_packet: &Ipv4Packet) {
        let ipv4_header = ipv4_packet.ipv4_header();
        assert_eq!(20, ipv4_header.header_length());
        assert_eq!(32, ipv4_header.total_length());
        assert_eq!(Protocol::Udp, ipv4_header.protocol());
        assert_eq!(0x12345678, ipv4_header.source());
        assert_eq!(0x42424242, ipv4_header.destination());

        if let Some(TransportHeaderData::Udp(udp_header)) = ipv4_packet.transport_header_data() {
            assert_eq!(1234, udp_header.source_port());
            assert_eq!(5678, udp_header.destination_port());
        } else {
            panic!("No UDP transport header");
        }
    }

    fn check_another_packet_headers(ipv4_packet: &Ipv4Packet) {
        let ipv4_header = ipv4_packet.ipv4_header();
        assert_eq!(20, ipv4_header.header_length());
        assert_eq!(29, ipv4_header.total_length());
        assert_eq!(Protocol::Udp, ipv4_header.protocol());
        assert_eq!(0x11111111, ipv4_header.source());
        assert_eq!(0x22222222, ipv4_header.destination());

        if let Some(TransportHeaderData::Udp(udp_header)) = ipv4_packet.transport_header_data() {
            assert_eq!(1111, udp_header.source_port());
            assert_eq!(2222, udp_header.destination_port());
        } else {
            panic!("No UDP transport header");
        }
    }

    #[test]
    fn parse_ipv4_packet_buffer() {
        let raw = create_packet();
        let mut packet_buffer = Ipv4PacketBuffer::new();

        let mut cursor = io::Cursor::new(raw);
        packet_buffer.read_from(&mut cursor).unwrap();

        let packet = packet_buffer.as_ipv4_packet().unwrap();
        check_packet_headers(&packet);
    }

    #[test]
    fn parse_fragmented_ipv4_packet_buffer() {
        let raw = create_packet();
        let mut packet_buffer = Ipv4PacketBuffer::new();

        let mut cursor = io::Cursor::new(&raw[..14]);
        packet_buffer.read_from(&mut cursor).unwrap();

        assert!(packet_buffer.as_ipv4_packet().is_none());

        let mut cursor = io::Cursor::new(&raw[14..]);
        packet_buffer.read_from(&mut cursor).unwrap();

        let packet = packet_buffer.as_ipv4_packet().unwrap();
        check_packet_headers(&packet);
    }

    fn create_multi_packets() -> Vec<u8> {
        let mut raw = Vec::new();
        write_packet_to(&mut raw);
        write_another_packet_to(&mut raw);
        write_packet_to(&mut raw);
        raw
    }

    #[test]
    fn parse_multi_packets() {
        let raw = create_multi_packets();
        let mut packet_buffer = Ipv4PacketBuffer::new();

        let mut cursor = io::Cursor::new(raw);
        packet_buffer.read_from(&mut cursor).unwrap();

        check_packet_headers(&packet_buffer.as_ipv4_packet().unwrap());
        packet_buffer.next();
        check_another_packet_headers(&packet_buffer.as_ipv4_packet().unwrap());
        packet_buffer.next();
        check_packet_headers(&packet_buffer.as_ipv4_packet().unwrap());
        packet_buffer.next();

        assert!(packet_buffer.as_ipv4_packet().is_none());
    }
}
