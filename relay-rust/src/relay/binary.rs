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

use std::cmp::min;
use std::fmt::Write;

const MAX_STRING_PACKET_SIZE: usize = 20;

pub fn read_u16(data: &[u8]) -> u16 {
    u16::from_be_bytes([data[0], data[1]])
}

pub fn read_u32(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}

pub fn write_u16(data: &mut [u8], value: u16) {
    data[..2].copy_from_slice(&value.to_be_bytes());
}

pub fn write_u32(data: &mut [u8], value: u32) {
    data[..4].copy_from_slice(&value.to_be_bytes());
}

pub fn to_byte_array(value: u32) -> [u8; 4] {
    value.to_be_bytes()
}

pub fn build_packet_string(data: &[u8]) -> String {
    let mut s = String::new();
    let limit = min(MAX_STRING_PACKET_SIZE, data.len());
    for (i, &byte) in data.iter().take(limit).enumerate() {
        if i != 0 {
            let sep = if (i % 4) == 0 { "  " } else { " " };
            write!(&mut s, "{}", sep).unwrap();
        }
        write!(&mut s, "{:02X}", byte).unwrap();
    }
    if limit < data.len() {
        write!(&mut s, "  ... +{} bytes", data.len() - limit).unwrap();
    }
    s
}

// only compare the data part for fat pointers (ignore the vtable part)
// for some (buggy) reason, the vtable part may be different even if the data reference the same
// object
// See <https://github.com/Genymobile/gnirehtet/issues/61#issuecomment-370933770>
pub fn ptr_data_eq<T: ?Sized>(lhs: *const T, rhs: *const T) -> bool {
    // cast to thin pointers to ignore the vtable part
    lhs as *const () == rhs as *const ()
}
