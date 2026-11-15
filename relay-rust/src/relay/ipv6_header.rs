/*
 * Copyright (C) 2017 Genymobile
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
use super::ipv4_header::Protocol;
use std::mem;

pub const IPV6_HEADER_LENGTH: usize = 40;

pub struct Ipv6Header<'a> {
    raw: &'a [u8],
    data: &'a Ipv6HeaderData,
}

pub struct Ipv6HeaderMut<'a> {
    raw: &'a mut [u8],
    data: &'a mut Ipv6HeaderData,
}

#[derive(Clone)]
pub struct Ipv6HeaderData {
    payload_length: u16,
    next_header: Protocol,
    source: [u8; 16],
    destination: [u8; 16],
}

#[allow(dead_code)]
impl Ipv6HeaderData {
    pub fn parse(raw: &[u8]) -> Self {
        Self {
            payload_length: binary::read_u16(&raw[4..6]),
            next_header: match raw[6] {
                6 => Protocol::Tcp,
                17 => Protocol::Udp,
                _ => Protocol::Other,
            },
            source: {
                let mut addr = [0u8; 16];
                addr.copy_from_slice(&raw[8..24]);
                addr
            },
            destination: {
                let mut addr = [0u8; 16];
                addr.copy_from_slice(&raw[24..40]);
                addr
            },
        }
    }

    pub fn bind<'c, 'a: 'c, 'b: 'c>(&'a self, raw: &'b [u8]) -> Ipv6Header<'c> {
        Ipv6Header::new(raw, self)
    }

    pub fn bind_mut<'c, 'a: 'c, 'b: 'c>(&'a mut self, raw: &'b mut [u8]) -> Ipv6HeaderMut<'c> {
        Ipv6HeaderMut::new(raw, self)
    }

    pub fn header_length(&self) -> u8 {
        IPV6_HEADER_LENGTH as u8
    }

    pub fn payload_length(&self) -> u16 {
        self.payload_length
    }

    pub fn total_length(&self) -> u16 {
        // saturate: jumbo payloads (>65535-40) are not supported, treat as max
        (IPV6_HEADER_LENGTH as u32 + u32::from(self.payload_length)).min(u32::from(u16::MAX))
            as u16
    }

    pub fn protocol(&self) -> Protocol {
        self.next_header
    }

    pub fn next_header(&self) -> Protocol {
        self.next_header
    }

    pub fn source(&self) -> [u8; 16] {
        self.source
    }

    pub fn destination(&self) -> [u8; 16] {
        self.destination
    }
}

pub fn peek_version_length(raw: &[u8]) -> Option<(u8, u16)> {
    if raw.len() < IPV6_HEADER_LENGTH {
        // not enough data to know the payload length
        return None;
    }
    let version = raw[0] >> 4;
    if version != 6 {
        return None;
    }
    let payload_length = binary::read_u16(&raw[4..6]);
    let length =
        (IPV6_HEADER_LENGTH as u32 + u32::from(payload_length)).min(u32::from(u16::MAX)) as u16;
    Some((version, length))
}

// shared definition for Ipv6Header and Ipv6HeaderMut
macro_rules! ipv6_header_common {
    ($name:ident, $raw_type:ty, $data_type:ty) => {
        #[allow(dead_code)]
        impl<'a> $name<'a> {
            pub fn new(raw: $raw_type, data: $data_type) -> Self {
                Self { raw, data }
            }

            pub fn raw(&self) -> &[u8] {
                self.raw
            }

            pub fn data(&self) -> &Ipv6HeaderData {
                self.data
            }

            pub fn header_length(&self) -> u8 {
                self.data.header_length()
            }

            pub fn total_length(&self) -> u16 {
                self.data.total_length()
            }

            pub fn payload_length(&self) -> u16 {
                self.data.payload_length
            }

            pub fn protocol(&self) -> Protocol {
                self.data.next_header
            }

            pub fn source(&self) -> [u8; 16] {
                self.data.source
            }

            pub fn destination(&self) -> [u8; 16] {
                self.data.destination
            }
        }
    };
}

ipv6_header_common!(Ipv6Header, &'a [u8], &'a Ipv6HeaderData);
ipv6_header_common!(Ipv6HeaderMut, &'a mut [u8], &'a mut Ipv6HeaderData);

// additional methods for the mutable version
#[allow(dead_code)]
impl<'a> Ipv6HeaderMut<'a> {
    pub fn raw_mut(&mut self) -> &mut [u8] {
        self.raw
    }

    pub fn data_mut(&mut self) -> &mut Ipv6HeaderData {
        self.data
    }

    pub fn set_payload_length(&mut self, payload_length: u16) {
        self.data.payload_length = payload_length;
        binary::write_u16(&mut self.raw[4..6], payload_length);
    }

    pub fn set_source(&mut self, source: [u8; 16]) {
        self.data.source = source;
        self.raw[8..24].copy_from_slice(&source);
    }

    pub fn set_destination(&mut self, destination: [u8; 16]) {
        self.data.destination = destination;
        self.raw[24..40].copy_from_slice(&destination);
    }

    pub fn swap_source_and_destination(&mut self) {
        mem::swap(&mut self.data.source, &mut self.data.destination);
        for i in 8..24 {
            self.raw.swap(i, i + 16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_header() -> Vec<u8> {
        let mut raw: Vec<u8> = Vec::with_capacity(40);
        raw.push(6u8 << 4); // version + traffic class (high)
        raw.push(0); // traffic class (low) + flow label
        raw.extend_from_slice(&(0u16).to_be_bytes()); // flow label (remaining)
        raw.extend_from_slice(&(12u16).to_be_bytes()); // payload length 8 + 4
        raw.push(17); // next header (UDP)
        raw.push(64); // hop limit
        // source fd00::2
        raw.extend_from_slice(&[
            0xfd, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
        ]);
        // destination 2001:db8::1
        raw.extend_from_slice(&[
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ]);
        raw
    }

    #[test]
    fn parse_header() {
        let raw = &create_header()[..];
        let data = Ipv6HeaderData::parse(raw);
        assert_eq!(12, data.payload_length());
        assert_eq!(52, data.total_length());
        assert_eq!(Protocol::Udp, data.protocol());
        assert_eq!(
            [
                0xfd, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02
            ],
            data.source()
        );
    }

    #[test]
    fn edit_header() {
        let raw = &mut create_header()[..];
        let mut header_data = Ipv6HeaderData::parse(raw);
        let mut header = header_data.bind_mut(raw);

        let src = [1u8; 16];
        let dst = [2u8; 16];
        header.set_source(src);
        header.set_destination(dst);
        header.set_payload_length(42);
        assert_eq!(src, header.source());
        assert_eq!(dst, header.destination());
        assert_eq!(42, header.payload_length());
        assert_eq!(82, header.total_length());

        header.swap_source_and_destination();
        assert_eq!(dst, header.source());
        assert_eq!(src, header.destination());
    }

    #[test]
    fn peek_length() {
        let raw = create_header();
        let (version, length) = peek_version_length(&raw).unwrap();
        assert_eq!(6, version);
        assert_eq!(52, length);
        let empty: [u8; 0] = [];
        assert!(peek_version_length(&empty).is_none());
        let short = [0x60u8, 0, 0];
        assert!(peek_version_length(&short).is_none());
    }
}
