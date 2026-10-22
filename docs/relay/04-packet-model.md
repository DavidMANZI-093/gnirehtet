# 04 — Packet model and forging

[← Previous: tunnel and client](03-tunnel-and-client.md) · [Index](README.md) · [Next: TCP proxy →](05-tcp-proxy.md)

This chapter covers the byte-level model: how the relay parses IPv4, TCP, and UDP
headers, how it recovers packet boundaries from the tunnel stream, how checksums
are computed, and how `Packetizer` builds the response packets the device sees.

The transport engines that use these primitives are documented in
[05](05-tcp-proxy.md) and [06](06-udp-and-buffering.md).

## 1. The layering convention

The source uses a "level" comment in `Packetizer`:

> Convert from level 5 to level 3 by appending correct IP and transport headers.

In this codebase:

- **Level 5** means application payload bytes (TCP stream bytes or one UDP
  datagram payload).
- **Level 4** means the TCP/UDP header plus payload.
- **Level 3** means the IPv4 header plus transport packet.

The parser goes in the opposite direction: it receives level 3 bytes and exposes
headers plus payload. The relay does not model Ethernet, ARP, or IPv6. It parses
only the IPv4 fields it needs; IP options are not interpreted, but the reference
header bytes are copied verbatim when a response is forged.

## 2. Raw IPv4 packet layout

A normal IPv4 packet with a 20-byte header:

```text
offset  size  field
0       1     version (high nibble) + IHL (low nibble)
1       1     DSCP/ECN (ToS)
2       2     total length (bytes, big-endian)
4       2     identification
6       2     flags + fragment offset
8       1     TTL
9       1     protocol (6 = TCP, 17 = UDP)
10      2     header checksum
12      4     source address
16      4     destination address
20      ...   options (if IHL > 5), then transport header + payload
```

`Ipv4HeaderData::parse` extracts:

```rust
_version:        raw[0] >> 4,
header_length:   (raw[0] & 0xf) << 2,       // IHL × 4 bytes
total_length:    read_u16(&raw[2..4]),
protocol:        6 => Tcp, 17 => Udp, _ => Other,
source:          read_u32(&raw[12..16]),
destination:     read_u32(&raw[16..20]),
```

It does **not** store or validate the checksum, flags, identification, TTL, or
ToS. Those bytes remain in the raw buffer and are copied by the packetizer when a
response is forged.

Addresses are stored as `u32` in the numeric value produced by big-endian
decoding. For example, `10.0.0.2` becomes `0x0A000002`. `net::to_addr` converts
back with `Ipv4Addr::from(u32)`; `net::to_socket_addr` adds a port.

### 2.1 Header views versus header data

The code separates **parsed data** from **raw views**:

| Type | Purpose |
|------|---------|
| `Ipv4HeaderData` | Owned, cloneable field values; no lifetime. |
| `Ipv4Header<'a>` | Read-only view binding `&[u8]` and `&Ipv4HeaderData`. |
| `Ipv4HeaderMut<'a>` | Mutable view binding `&mut [u8]` and `&mut Ipv4HeaderData`. |

`bind`/`bind_mut` create a view. Mutating through `Ipv4HeaderMut` updates both the
parsed field and the raw bytes, keeping the two representations consistent.

`Ipv4HeaderMut` supports:

- `set_total_length`
- `set_source`, `set_destination`
- `swap_source_and_destination`
- `update_checksum`

`swap_source_and_destination` swaps the `u32` fields and swaps raw bytes 12..16
with 16..20. It does not update the checksum; the packetizer calls
`compute_checksums` after all mutations.

### 2.2 IPv4 checksum

`Ipv4HeaderMut::update_checksum`:

1. Computes the number of 16-bit words from `header_length / 2`.
2. Sums every word except the checksum word at bytes 10..12.
3. Folds carries: `sum = (sum & 0xffff) + (sum >> 16)` until no high bits remain.
4. Writes the one's complement (`!sum`) into bytes 10..12.

The implementation assumes the header length is even (IHL × 4 is always a
multiple of 4, so this holds for valid headers) and that the raw slice contains at
least the header. It does not validate the input checksum.

### 2.3 Version/length peek

`ipv4_header::peek_version_length(raw)` returns `None` if fewer than four bytes
are available. Otherwise it returns:

```text
(version = raw[0] >> 4, length = read_u16(raw[2..4]))
```

This is the only framing information needed to split the device stream.

## 3. Transport header dispatch

`TransportHeaderData` is an enum:

```rust
enum TransportHeaderData {
    Tcp(TcpHeaderData),
    Udp(UdpHeaderData),
}
```

`TransportHeaderData::parse(protocol, raw)`:

- `Udp` → `UdpHeaderData::parse(raw)`
- `Tcp` → `TcpHeaderData::parse(raw)`
- anything else → `None`

`TransportHeader`/`TransportHeaderMut` are matching enums of header views. They
provide common methods:

- `source_port`, `destination_port`
- `header_length`
- `raw`, `data_clone`
- `swap_source_and_destination`
- `set_payload_length` (UDP only; TCP is a no-op)
- `update_checksum`

`header_length` comes from the TCP data offset for TCP and is always 8 for UDP.
This is what lets `Ipv4Packet::split` find the payload boundary.

## 4. TCP header

`TcpHeaderData::parse` reads:

```text
source_port             raw[0..2]
destination_port        raw[2..4]
sequence_number         raw[4..8]
acknowledgement_number  raw[8..12]
header_length           ((raw[12..14] as u16 & 0xF000) >> 10)   // data offset × 4
flags                   raw[12..14] & 0x1FF
window                  raw[14..16]
```

The "data offset and flags" field packs the 4-bit data offset in the high nibble
and nine flag bits in the low bits. The parser keeps only the flags the relay
needs to test:

| Constant | Value | Meaning |
|----------|-------|---------|
| `FLAG_FIN` | `1 << 0` | Finish |
| `FLAG_SYN` | `1 << 1` | Synchronize sequence numbers |
| `FLAG_RST` | `1 << 2` | Reset |
| `FLAG_PSH` | `1 << 3` | Push |
| `FLAG_ACK` | `1 << 4` | Acknowledgment valid |

The header also contains a 16-bit window and a 16-bit checksum, both accessible
through the view.

### 4.1 Mutable operations

`TcpHeaderMut` supports:

- `set_source_port`, `set_destination_port`
- `swap_source_and_destination` (ports and raw bytes)
- `set_sequence_number`
- `set_acknowledgement_number`
- `set_flags` (preserves the data offset/reserved bits, replaces the low 9 bits)
- `shrink_options` (sets data offset to 5, i.e. 20-byte header)
- `update_checksum`

`shrink_options` is used when a TCP connection is created: the relay copies the
first 20 bytes of the client's TCP header, forces a 20-byte header, and saves that
as the reference header. The device's TCP options (MSS, window scale, SACK, etc.)
are therefore not echoed in forged packets. The relay does not need to negotiate
options because it is not a real TCP endpoint for the device; it only needs a
coherent minimal header.

### 4.2 TCP checksum

TCP checksumming uses the IPv4 pseudo-header (RFC 793 §3.1):

```text
source address
destination address
zero + protocol (6)
TCP length = IP total length - IP header length
TCP header (checksum field set to zero)
payload
```

`TcpHeaderMut::update_checksum`:

1. Reads source/destination from `Ipv4HeaderData`.
2. Computes `transport_length = total_length - header_length`.
3. Computes `payload_length = transport_length - tcp_header_length`.
4. Debug-asserts that the header length is even and at least 20, and that the
   payload length matches the actual payload slice.
5. Seeds the sum with protocol 6, source high/low halves, destination high/low
   halves, and transport length.
6. Sets the checksum field to zero.
7. Sums the header and payload 16-bit words; an odd trailing payload byte is
   treated as the high byte of a final word.
8. Folds carries and writes the one's complement.

The implementation uses unsafe pointer walks for speed, with a comment that TCP
checksum computation is the most CPU-intensive task in Gnirehtet. It is a
deliberate optimization; the surrounding debug assertions are the safety net for
internal consistency.

The checksum is computed for every forged TCP packet, including empty
SYN/ACK/FIN/RST packets and data packets with `ACK|PSH`.

## 5. UDP header

UDP is simpler:

```text
source_port       raw[0..2]
destination_port  raw[2..4]
length            raw[4..6]   (header + payload)
checksum          raw[6..8]
```

`UdpHeaderData` stores only source and destination ports. `UdpHeaderMut` can swap
them and set the length:

```rust
fn set_payload_length(&mut self, payload_length: u16) {
    self.raw[4..6] = (8 + payload_length).to_be_bytes();
}
```

### 5.1 UDP checksum is disabled

`UdpHeaderMut::update_checksum` ignores the IPv4 header and payload and writes
zero into the checksum field:

```rust
// disable checksum validation
self.set_checksum(0);
```

For IPv4, a UDP checksum of zero means "no checksum" and is legal. The Android
client/device accepts these packets. This removes the cost of checksumming every
UDP datagram and avoids having to handle the UDP pseudo-header. It also means the
relay does not detect corruption in the forged UDP packets; the local link and
the client are trusted.

## 6. `Ipv4Packet`: a parsed packet view

`Ipv4Packet<'a>` holds:

```rust
raw: &'a mut [u8],
ipv4_header_data: Ipv4HeaderData,
transport_header_data: Option<TransportHeaderData>,
```

### 6.1 Parsing

`Ipv4Packet::parse(raw)`:

1. Parses the IPv4 header from the front of `raw`.
2. Takes the slice starting at `header_length` and parses TCP/UDP if the protocol
   is recognized.
3. Trims `raw` to `total_length` bytes.

The trim is important: the buffer may contain multiple packets or trailing bytes;
the packet view should expose exactly the declared packet.

The parse functions assume the raw slice is large enough for every field they
read. In normal operation the caller (`Ipv4PacketBuffer`) has already verified
that the full declared length is present, and the Android client supplies valid
headers. This is a trust boundary, not a hardened parser.

### 6.2 Accessors

| Method | Returns |
|--------|---------|
| `raw()` | The full packet bytes, trimmed to total length |
| `headers_data()` | `(&Ipv4HeaderData, Option<&TransportHeaderData>)` |
| `headers()` | Header views |
| `ipv4_header_data()` / `ipv4_header()` | IPv4-only views |
| `transport_header_data()` / `transport_header()` | TCP/UDP-only views |
| `split()` / `split_mut()` | IPv4 header and optional (transport header, payload) |
| `is_valid()` | Whether a TCP/UDP transport header was recognized |
| `length()` | IPv4 total length |
| `payload()` | Transport payload slice |
| `compute_checksums()` | Recompute IPv4 and transport checksums |

`compute_checksums` splits the packet mutably, updates the IPv4 checksum, then
passes the IPv4 data and payload to the transport header's checksum routine. This
is the normal final step after a packet has been forged or modified.

### 6.3 Why split matters

Rust's borrow rules make it awkward to hold a mutable IPv4 header view and a
mutable transport header view at the same time from the same raw slice.
`split_mut` performs the split once and returns non-overlapping mutable views:

```rust
let (mut ipv4_header, transport) = packet.split_mut();
ipv4_header.update_checksum();
if let Some((mut transport_header, payload)) = transport {
    transport_header.update_checksum(ipv4_header.data(), payload);
}
```

This pattern is used by `compute_checksums` and by tests. It also explains some of
the lifetimes in `Ipv4Packet`; the data structs are separate from the raw bytes so
a view can be reconstructed without re-parsing.

## 7. `Ipv4PacketBuffer`: recovering packet boundaries

`Ipv4PacketBuffer` wraps a `ByteBuffer` of `MAX_PACKET_LENGTH` (65536) bytes.

### 7.1 Reading

`read_from` delegates to `ByteBuffer::read_from`, which reads into the unused
portion at the end and returns whether any bytes were read.

### 7.2 Available packet length

`available_packet_length`:

1. Peeks at the accumulated bytes.
2. If fewer than 4 bytes, returns `None` (not enough for version + length).
3. Asserts `version == 4`.
4. If `length <= data.len()`, returns `Some(length)`.
5. Otherwise returns `None` (partial packet).

`as_ipv4_packet` returns `Some(Ipv4Packet::parse(data))` only when a complete
packet is available; otherwise `None`. `next` re-evaluates the length and consumes
exactly that many bytes from the front of the buffer.

### 7.3 Framing properties

- The stream may deliver any chunking. A packet can be split across many reads or
  several packets can arrive in one read.
- The total-length field is authoritative. There is no separate length prefix.
- A declared total length of zero is not special-cased. `next` would consume zero
  bytes forever, so the caller would spin; valid IPv4 packets always have a
  length of at least 20.
- A version other than 4 panics. This is an intentional invariant: the Android
  client filters non-IPv4 packets before sending them.
- The buffer is linear, not circular; `ByteBuffer::consume` compacts the remaining
  bytes to the front. The maximum buffer size is one byte larger than the maximum
  IPv4 total length.

### 7.4 No IP reassembly

The relay does not reassemble IPv4 fragments. It treats the bytes after the IPv4
header as a TCP/UDP header based solely on the protocol number. For a non-initial
fragment, those bytes are not a transport header, so port parsing would be wrong.
In practice the Android VPN MTU (`0x4000`) makes fragmentation unlikely for TCP
and normal UDP, but large UDP datagrams or unusual network conditions can still
fragment. This is a known limitation, not a supported feature.

## 8. `Packetizer`: forging the response packet

`Packetizer` is the inverse of parsing. It owns a 65536-byte buffer and a pair of
reference header data clones.

```rust
pub struct Packetizer {
    buffer: Box<[u8; MAX_PACKET_LENGTH]>,
    transport_index: usize,
    payload_index: usize,
    ipv4_header_data: Ipv4HeaderData,
    transport_header_data: TransportHeaderData,
}
```

### 8.1 Construction

`Packetizer::new(reference_ipv4_header, reference_transport_header)`:

1. Computes `transport_index = reference_ipv4_header.header_length()`.
2. Computes `payload_index = transport_index + reference_transport_header.header_length()`.
3. Clones the header data.
4. Copies the reference IPv4 header into `buffer[..transport_index]`, binds a
   mutable view, and swaps source/destination.
5. Copies the reference transport header into
   `buffer[transport_index..payload_index]`, binds a mutable view, and swaps
   source/destination ports.
6. Stores the indexes and data clones.

The payload area is `buffer[payload_index..]`. The reference payload is **not**
copied; only headers are retained. The buffer is reused for every outgoing packet
of that flow.

The reference header comes from the first packet of the flow:

- TCP: the first SYN, with TCP options shrunk to 20 bytes.
- UDP: the first datagram.

This means the response IP header preserves most fields from the request (ToS,
identification, TTL, flags/fragment offset), while addresses are reversed. For
TCP, the response header is always 20 bytes with no options. For UDP, the header
is 8 bytes.

### 8.2 Building a packet

`build(payload_length)`:

```rust
let total_length = self.payload_index as u16 + payload_length;
self.ipv4_header_mut().set_total_length(total_length);
self.transport_header_mut().set_payload_length(payload_length);
let mut packet = Ipv4Packet::new(
    &mut self.buffer[..total_length as usize],
    self.ipv4_header_data.clone(),
    self.transport_header_data.clone(),
);
packet.compute_checksums();
packet
```

`set_payload_length` is a no-op for TCP; the IP total length is what determines
the TCP length. For UDP it writes `8 + payload_length` into the UDP length field.

`compute_checksums` updates the IPv4 checksum and then either computes the TCP
checksum or writes zero for UDP.

### 8.3 Four entry points

| Entry point | Source | Use |
|-------------|--------|-----|
| `packetize_empty_payload()` | none | SYN, ACK, FIN, RST control packets |
| `packetize(&mut DatagramReceiver)` | one UDP datagram | `UdpConnection` response |
| `packetize_read(&mut Read, max_chunk_size)` | a stream read | `TcpConnection` response |
| `inflate(packet_length)` | already-built buffer | deferred TCP packet |

`packetize_read` wraps the stream in `ReadAdapter`, which limits the read to
`min(max_chunk_size, destination_slice.len())`; the destination is the packetizer's
remaining payload area, not the whole 64 KiB allocation. A zero-byte read is
treated as EOF and returns `Ok(None)`; any positive read builds a packet. This is
how TCP turns a byte stream into MTU-sized packets.

`inflate` does **not** rebuild or checksum the packet. It constructs an
`Ipv4Packet` view over the already-built bytes, using the stored header data. It
is used when a response packet was built but could not be enqueued on the client;
the exact bytes are retained and later handed to the client.

### 8.4 Payload limits and MTU

`Packetizer`'s buffer is 65536 bytes, the same as `MAX_PACKET_LENGTH`. The TCP
connection limits reads to:

```text
MTU = 0x4000 = 16384
MAX_PAYLOAD_LENGTH = MTU - 20 (IP) - 20 (TCP) = 16344
```

This matches the Android client's MTU. TCP responses therefore carry at most
16344 payload bytes, plus 40 bytes of headers, for a 16384-byte IP packet. IP
total length is comfortably below 65535.

UDP does not use the MTU constant directly. A real UDP datagram is at most 65507
payload bytes; the packetizer's buffer has room for the 8-byte header plus that
payload. The `MAX_DATAGRAM_LENGTH` constant in `datagram.rs` is 65536, used as a
buffer bound; actual UDP payload size is limited by the socket and the IP total
length field.

### 8.5 A worked example

Device packet:

```text
IPv4: 10.0.0.2 -> 93.184.216.34
TCP:  54321 -> 80, seq=1000, ack=2000, flags=ACK|PSH, payload="GET ..."
```

When the first SYN was seen, the packetizer saved a reference IPv4 header with
source `10.0.0.2` and destination `93.184.216.34`, and a TCP header with ports
`54321 -> 80`. Construction swaps them:

```text
IPv4: 93.184.216.34 -> 10.0.0.2
TCP:  80 -> 54321
```

For each response, `TcpConnection::update_headers` sets the sequence number to
the relay's current server sequence, the acknowledgement number to the next
expected device byte, and the flags. `build` sets total length and computes
checksums. The payload is whatever was read from the host socket.

For a UDP response, the same address/port reversal happens, the UDP length is set
to `8 + payload`, and the UDP checksum is zero.

## 9. Checksums in one table

| Packet part | Incoming | Outgoing (forged) |
|-------------|----------|-------------------|
| IPv4 header | Not verified | Recomputed by `Ipv4HeaderMut::update_checksum` |
| TCP | Not verified | Recomputed over pseudo-header + header + payload |
| UDP | Not verified | Set to zero (disabled for IPv4) |

The relay trusts the local client's incoming packets. It does not reject a packet
because its IPv4 or TCP checksum is wrong. This is consistent with the trust
model and saves work in the request path. The device's kernel is expected to have
already validated packets before handing them to the VPN app, and the adb link is
local.

## 10. Binary helpers

`binary.rs` contains small helpers used by headers, buffers, and logs:

| Function | Purpose |
|----------|---------|
| `read_u16`, `read_u32` | Big-endian decode |
| `write_u16`, `write_u32` | Big-endian encode |
| `to_byte_array(u32)` | Big-endian 4-byte id |
| `build_packet_string(data)` | Hex dump of first 20 bytes, grouped by 4; appends `... +N bytes` |
| `ptr_data_eq(lhs, rhs)` | Compare thin data pointers, ignoring fat-pointer vtables |

`build_packet_string` is used at `Trace` level for packet dumps and for warnings
about invalid packets. It allocates a `String`; it is guarded by
`logger::is_enabled(LevelFilter::Trace)` where the cost matters.

`ptr_data_eq` exists because `Rc<RefCell<dyn Connection>>` is a fat pointer. The
data pointer identifies the object; the vtable pointer can differ for references
to the same object in some situations (the comment cites a historical issue).
Casting to `*const ()` compares only the address.

## 11. Invariants and limitations of the codec

- **IPv4 only.** Version 4 is asserted when framing; version 6 is not modeled.
- **Valid header sizes assumed.** IHL must be at least 5 and the total length must
  fit the buffer; the parser does not reject malformed values gracefully.
- **No fragment reassembly.** Protocol parsing assumes the transport header is
  present at the start of the post-IP payload.
- **No incoming checksum verification.** The relay trusts the device and adb.
- **TCP options stripped.** Forged TCP headers are always 20 bytes.
- **UDP checksum disabled.** Forged UDP headers use checksum zero.
- **IP ID/TTL/ToS copied.** Responses reuse the reference IP header fields rather
  than generating fresh values.
- **Response window copied.** Forged TCP packets keep the window field from the
  reference header; the relay does not implement a separate receive window.

These are deliberate simplifications appropriate to a local, trusted,
single-device proxy. They are also the first places to revisit if the relay is
ever exposed to untrusted input or extended beyond the current Android client.

Continue with [05 — TCP proxy](05-tcp-proxy.md) to see how these primitives are
sequenced into a working TCP conversation.
