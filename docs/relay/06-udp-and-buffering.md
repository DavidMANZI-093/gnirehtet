# 06 — UDP proxy and buffers

[← Previous: TCP proxy](05-tcp-proxy.md) · [Index](README.md) · [Next: lifecycle and diagnostics →](07-lifecycle-errors-and-diagnostics.md)

UDP is much simpler than TCP: there is no handshake, no sequence space, no
acknowledgment, and no connection state beyond an idle timer. The hard part is
preserving **datagram boundaries** while still using nonblocking sockets and
bounded buffers. This chapter covers the UDP engine and then the four buffer
primitives that the whole relay relies on.

## 1. UDP model

For each UDP flow, the relay creates one connected host `UdpSocket`. A flow is
identified by:

```text
protocol = UDP
source IP + source port        (the device side)
destination IP + destination port (the remote side)
```

The relay forwards each device datagram as one `send` on the host socket, and each
host datagram as one forged IPv4 packet to the device. It does not combine or
split datagrams.

UDP is connectionless on the wire, but `UdpSocket::connect` is useful here:

- it fixes the remote address, so `send`/`recv` need no address arguments;
- the kernel can report ICMP errors on the socket;
- the relay's flow key is naturally one connected socket per 4-tuple.

## 2. `UdpConnection`

```rust
pub struct UdpConnection {
    id: ConnectionId,
    client: Weak<RefCell<Client>>,
    socket: UdpSocket,
    interests: Interest,
    token: Token,
    client_to_network: DatagramBuffer,
    network_to_client: Packetizer,
    closed: bool,
    idle_since: Instant,
}
```

| Field | Role |
|-------|------|
| `id` | Flow key and log prefix. |
| `client` | Weak reference used to send responses and remove from router. |
| `socket` | Connected nonblocking host UDP socket. |
| `interests` | Current `mio` interest (always includes `READABLE`). |
| `token` | Event-loop token. |
| `client_to_network` | Queue of device datagrams waiting to be sent to the host. |
| `network_to_client` | Packetizer for host datagrams. |
| `closed` | Terminal flag. |
| `idle_since` | Last activity time for expiration. |

`IDLE_TIMEOUT_SECONDS` is `2 * 60 = 120`.

### 2.1 Creation

`UdpConnection::create`:

1. Logs `Open`.
2. Binds a host UDP socket to `0.0.0.0:0` (kernel-chosen ephemeral port).
3. Connects it to `id.rewritten_destination()` (with the `10.0.2.2 → 127.0.0.1`
   rewrite).
4. Creates a `Packetizer` from the first device packet's IPv4/UDP headers.
5. Sets interests to `READABLE`.
6. Builds the `Rc<RefCell<Self>>`, registers the socket with a handler, stores the
   token.
7. Returns the connection.

`UdpSocket::bind` and `connect` are nonblocking because `mio` creates nonblocking
sockets. If either fails, the router logs the failure and drops the packet; no
connection is created.

The host source port is chosen by the OS and is unrelated to the device's source
port. The device only sees the forged response packet, whose source/destination
ports are reversed from the request by the packetizer.

### 2.2 Readiness processing

`UdpConnection::on_ready` calls `process`. `WouldBlock` is treated as a spurious
event; other errors panic at this boundary, but internal socket errors usually
close the connection first.

`process`:

```text
if closed: return
touch()                          // update idle_since
if readable or writable:
    if writable: process_send()
    if not closed and readable: process_receive()
    if not closed: update_interests()
else:
    close()                      // error or hup
if closed: remove_from_router()
```

`touch` runs on every event, including a read or write attempt that ends in
`WouldBlock`. The idle timer therefore measures event activity, not successful
payload transfer. A socket that repeatedly becomes readable with errors will stay
alive until an error closes it.

### 2.3 Device → host

`Connection::send_to_network` is called by the router when a device datagram
arrives:

```rust
match self.client_to_network.read_from(ipv4_packet.payload().expect("No payload")) {
    Ok(()) => self.update_interests(selector),
    Err(err) => warn!("Cannot send to network, drop packet: {}", err),
}
```

`DatagramBuffer::read_from`:

- asserts the payload is at most `MAX_DATAGRAM_LENGTH` (65536);
- checks `has_enough_space_for`;
- writes a 2-byte length header plus the payload;
- advances `head`, wrapping at `circular_buffer_length`.

If the buffer is full, the datagram is dropped and a warning is logged. UDP has no
retransmission, so there is no recovery mechanism; the application loses that
datagram.

`update_interests` switches to `READABLE | WRITABLE` when the buffer is non-empty,
so the event loop knows to try sending.

### 2.4 Sending to the host

`process_send` loops while the datagram buffer is non-empty:

```text
write_to(socket)
  Ok    → datagram fully consumed
  WouldBlock → break, keep datagram queued
  error → log, close
```

`DatagramBuffer::write_to` reads the 2-byte length, sends exactly that many bytes,
and advances `tail` only if the socket accepted the whole datagram. This is the
critical nonblocking behavior:

- A `WouldBlock` from `send` returns before `tail` is advanced, so the datagram
  remains at the front of the queue.
- A short send is treated as an error (`write the whole datagram`). UDP `send` is
  normally all-or-nothing; a partial result would indicate an unexpected socket
  implementation.
- Once the datagram is sent, `tail` advances past the header and payload, wrapping
  at `circular_buffer_length`.

The test suite includes `preserve_datagram_on_would_block`, which specifically
verifies that a blocked send does not lose the queued datagram.

### 2.5 Host → device

`process_receive` loops:

```text
packetize(socket)             // recv one datagram into Packetizer buffer
send_to_client(packet)
  Ok    → log; optionally trace hex
  Err   → warn "Cannot send to client, drop packet"
```

`Packetizer::packetize` calls `recv` with a buffer starting at the payload index.
A successful `recv` returns the datagram length; `build` sets the IP total length,
UDP length, IPv4 checksum, and UDP checksum (zero). The packet is then enqueued on
the client if its stream buffer has room.

Unlike TCP, there is no pending-source mechanism for UDP. If the client buffer is
full, the response datagram is dropped. This is acceptable for UDP semantics but
means a burst can lose packets under backpressure.

`process_receive` loops until `recv` returns `WouldBlock`; it does not stop after
one datagram. This drains edge-triggered readiness.

### 2.6 Interests

UDP interests are:

```text
client_to_network empty      → READABLE
client_to_network non-empty  → READABLE | WRITABLE
```

The socket remains readable at all times so the relay can receive responses even
while it has queued outgoing datagrams. Writable is added when a queued datagram
could not be sent immediately (for example, the host socket buffer is full).

### 2.7 Idle expiration

`touch` sets `idle_since = Instant::now()` at the start of every `process` call.
`is_expired` returns:

```rust
self.idle_since.elapsed().as_secs() > IDLE_TIMEOUT_SECONDS
```

The strict `>` means exactly 120 seconds is not yet expired. Cleanup runs every 60
seconds, so a flow that goes idle just after a cleanup can survive for nearly 180
seconds. On expiration:

1. `Router::clean_expired_connections` logs and calls `close`.
2. `UdpConnection::close` logs `Close`, sets `closed`, deregisters the socket
   (warning on failure), and drops the socket by RAII.
3. The router `swap_remove`s the connection.

There is no idle timeout for TCP; see [07](07-lifecycle-errors-and-diagnostics.md).

## 3. `DatagramBuffer` in detail

`DatagramBuffer` is a circular queue of length-prefixed datagrams. It preserves
boundaries, unlike `StreamBuffer`, which is a byte stream.

### 3.1 Layout

```text
circular_buffer_length
|<------------------------->| extra space for a maximum block
+---------------------------+---------------------------+
| [len][D4] [len][D1] ...    | [len][max datagram]       |
+---------------------------+---------------------------+
      ^     ^
    head    tail
```

- `buf.len() = capacity + MAX_BLOCK_LENGTH`.
- `circular_buffer_length = capacity + 1`.
- `MAX_BLOCK_LENGTH = 2 + MAX_DATAGRAM_LENGTH`.
- A block is `[2-byte big-endian length][payload]`.

The extra `MAX_BLOCK_LENGTH` region allows the last block to be written
contiguously even if it crosses the nominal circular boundary. This avoids
splitting a datagram header from its payload across the wrap point.

### 3.2 Capacity test

```rust
if self.head >= self.tail {
    return true;
}
let remaining = self.tail - self.head - 1;
HEADER_LENGTH + datagram_length <= remaining
```

The `head >= tail` case covers an empty queue and the forward layout where the
write head is at or ahead of the read tail. Rather than compute the free space
across the end of the nominal ring, the code returns true and relies on the extra
region to hold one block. The `head < tail` case computes the free space between
the write head and the read tail, excluding one byte used to distinguish empty
from full (the same sentinel idea as `StreamBuffer`).

The `head >= tail` branch is deliberately permissive rather than a full capacity
calculation. It admits one block using the extra region; it does not independently
prove that an arbitrarily large burst fits. Normal UDP operation drains the queue
in FIFO order, and the extra block is sized for the largest possible datagram, but
the check should be read as a one-block safety margin, not as a general "space for
N blocks" guarantee. In the unlucky case where `tail` is zero and the write head
wraps to zero while using the extra region, `is_empty` also sees `head == tail`,
so the queue can appear empty even though a block was written. This is an edge
case in the capacity accounting, not a supported full-buffer behavior.

### 3.3 Write and read indices

- Write (`read_from`): write the 2-byte length at `head`, copy the payload after
  it, advance `head` by `2 + length`, then if `head >= circular_buffer_length`,
  set `head = 0`.
- Read (`write_to`): read 2-byte length at `tail`, send exactly `length` bytes
  from `tail + 2`, advance `tail` to `tail + 2 + length`, then wrap to 0 if
  `tail >= circular_buffer_length`.

Because the block is contiguous (thanks to the extra space), no split copy is
needed.

### 3.4 Full and empty behavior

- `is_empty`: `head == tail`.
- `read_from` returns `Err("Datagram buffer is full")` if there is not enough
  room; the caller logs and drops the datagram.
- `write_to` asserts non-empty; callers check `is_empty` first.
- A zero-length datagram is allowed: the block is just the 2-byte length `0`. The
  tests cover zero-length datagrams and boundary preservation.

### 3.5 Nonblocking send guarantee

`write_to` calls `destination.send(source_slice)` and only advances `tail` after a
full-length success. This is why a `WouldBlock` leaves the queue unchanged. If a
partial send were ever returned, the code returns an error without advancing; the
datagram would remain queued, but the partial bytes may already have been sent.
For UDP, this is effectively impossible; the check is defensive.

## 4. `StreamBuffer`

`StreamBuffer` is a circular byte buffer for TCP/stream data. It does not preserve
message boundaries, which is exactly what TCP needs.

```rust
pub struct StreamBuffer {
    buf: Box<[u8]>,
    head: usize,   // next write position
    tail: usize,   // next read position
}
```

### 4.1 Capacity and empty/full

`new(capacity)` allocates `capacity + 1` bytes. The extra byte is a sentinel that
lets `head == tail` mean empty without also meaning full:

```text
size      = (head < tail) ? head + len - tail : head - tail
capacity  = buf.len() - 1
remaining = capacity - size
```

### 4.2 Writing to a socket

`write_to(destination)` writes one contiguous region:

- if `head > tail`, the data is `buf[tail..head]`;
- if `head < tail`, the data wraps, so it writes `buf[tail..]` only;
- if empty, returns `Ok(0)`.

It advances `tail` by the number of bytes written. If the buffer becomes empty,
`optimize` resets both indices to zero. Resetting avoids unnecessary future splits
and is especially useful when one packet is written per call: after the buffer
drains, the next packet starts at index zero.

A key consequence: `write_to` may write less than the full logical buffer even if
the destination could accept more, because the data is not contiguous. The caller
loops and writes again. `Client::process_send` and `TcpConnection::process_send`
both do this.

### 4.3 Reading from a source

`read_from(source: &[u8])` copies a byte slice into the circular buffer. It
asserts that `source.len() <= remaining()`; callers must check capacity first. It
handles wrap by splitting the copy into two `copy_from_slice` calls. It advances
`head` modulo `buf.len()`.

Unlike `DatagramBuffer`, there is no length prefix and no boundary preservation.
This is used for:

- `TcpConnection::client_to_network` (device payload to host socket);
- `Client::network_to_client` (forged packets to the tunnel).

### 4.4 Sizes

| Use | Capacity | Why |
|-----|----------|-----|
| `TcpConnection::client_to_network` | `4 * 65536` = 262,144 bytes | Absorb bursts of device payload while the host socket is not writable. |
| `Client::network_to_client` | `16 * 65536` = 1,048,576 bytes | Absorb many forged packets across all flows while the tunnel socket is not writable. |

Both are logical capacities; the underlying allocation is capacity + 1 due to the
sentinel.

## 5. `ByteBuffer`

`ByteBuffer` is a linear buffer with a read count (`head`):

```rust
pub struct ByteBuffer {
    buf: Box<[u8]>,
    head: usize,
}
```

- `read_from(source)` reads into `buf[head..]` and advances `head`.
- `peek()` returns `buf[..head]`.
- `peek_mut()` returns the mutable prefix.
- `remaining()` returns `buf.len() - head`.
- `consume(length)` checks `head >= length`, subtracts `length`, and if data
  remains, copies it from `buf[length..length+head]` to `buf[0..head]`.

It is not circular. Consumption compacts the remaining bytes to the front. This is
fine because it is used where packets are consumed one at a time and the cost of
compaction is small relative to packet handling:

- `Ipv4PacketBuffer` (64 KiB) for the device stream;
- `AdbMonitor` (1024 bytes) for the adb device-list stream.

The linear design makes packet parsing and `peek_mut` straightforward; there is no
wrap-around case in the packet parser.

## 6. `Ipv4PacketBuffer`

`Ipv4PacketBuffer` is a thin framing layer over a 64 KiB `ByteBuffer`. It is
documented in detail in [04 — Packet model](04-packet-model.md#7-ipv4packetbuffer-recovering-packet-boundaries).
In summary:

- read raw bytes until a complete IPv4 packet is available;
- use bytes 0..4 for version and total length;
- assert version 4;
- parse the packet with `Ipv4Packet::parse`;
- consume exactly the declared length.

It does not add length prefixes, does not handle IPv6, and does not reassemble IP
fragments. The Android client is expected to send only complete IPv4 packets.

## 7. `ReadAdapter` and datagram traits

`datagram.rs` defines two small traits so the packetizer can read from either a
UDP socket or a TCP stream:

```rust
trait DatagramSender { fn send(&mut self, buf: &[u8]) -> io::Result<usize>; }
trait DatagramReceiver { fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize>; }
```

`UdpSocket` implements both. `ReadAdapter<R>` adapts an `io::Read` to
`DatagramReceiver`, optionally limiting each `recv` to `max_chunk_size`:

```rust
let len = min(max_chunk_size, buf.len());
self.read.read(&mut buf[..len])
```

This is how `Packetizer::packetize_read` turns a TCP stream read into one packet
without accidentally reading more than the MTU allows. `ReadAdapter` is
constructed per read in `packetize_read`; it is cheap.

The `tests` module also defines `MockDatagramSocket`, used by packetizer and
datagram-buffer tests.

## 8. Buffer summary table

| Buffer | Type | Capacity | Framing | Primary use |
|--------|------|----------|---------|-------------|
| `ByteBuffer` | linear bytes | caller-specified | none | ADB monitor, `Ipv4PacketBuffer` |
| `Ipv4PacketBuffer` | linear + IPv4 length | 65,536 | IPv4 packet | Device → relay framing |
| `StreamBuffer` | circular bytes | caller-specified | byte stream | TCP payload, client egress |
| `DatagramBuffer` | circular blocks | 4 × 65,536 logical | datagram | UDP device → host queue |
| `Packetizer` buffer | linear bytes | 65,536 | one forged packet | Header + response payload |

### 8.1 Exact constants

| Constant | Value | Where |
|----------|-------|-------|
| `MAX_PACKET_LENGTH` | `1 << 16` = 65,536 | `ipv4_packet.rs` |
| `MAX_DATAGRAM_LENGTH` | `1 << 16` = 65,536 | `datagram.rs` |
| `MAX_BLOCK_LENGTH` | `2 + 65,536` = 65,538 | `datagram_buffer.rs` |
| `HEADER_LENGTH` | 2 | `datagram_buffer.rs` |
| `MTU` | `0x4000` = 16,384 | `tcp_connection.rs` |
| `MAX_PAYLOAD_LENGTH` | 16,344 | `tcp_connection.rs` |
| TCP `client_to_network` capacity | 262,144 | `tcp_connection.rs` |
| Client `network_to_client` capacity | 1,048,576 | `client.rs` |
| UDP `client_to_network` capacity | 262,144 logical | `udp_connection.rs` |
| ADB monitor buffer | 1,024 | `adb_monitor.rs` |

## 9. Backpressure behavior by buffer

The relay's memory is bounded, and each buffer has an explicit response to
pressure:

| Situation | Buffer | Behavior |
|-----------|--------|----------|
| Device sends faster than host TCP accepts | TCP `client_to_network` | If full, drop the segment; device retransmits. |
| Host sends faster than tunnel accepts | Client `network_to_client` | TCP: defer one packet and stop reading host. UDP: drop the datagram. |
| Tunnel sends faster than device TCP accepts | Device's own window | TCP: stop reading host until window opens. |
| Host UDP socket not writable | UDP `DatagramBuffer` | Keep datagram; retry on writable. If queue full, drop new datagrams. |
| Device stream contains partial packet | `Ipv4PacketBuffer` | Wait for more bytes; do not consume. |
| ADB track-devices stream is fragmented | `ByteBuffer` | Wait until 4-byte length and full payload arrive. |

The design intentionally avoids unbounded queues. Under sustained overload, data
is lost according to protocol semantics rather than growing memory without limit.

## 10. Why datagram boundaries are preserved

TCP is a byte stream; an application's `write` boundaries are not preserved. UDP
is datagram-oriented; each `send` must arrive as one `recv` with the same length.
The relay's UDP path therefore cannot use `StreamBuffer` or a simple byte queue.

`DatagramBuffer` stores a length before each payload and only advances the read
tail after a complete send. This ensures:

- two device datagrams are sent as two host datagrams;
- a zero-length datagram remains a zero-length datagram;
- a `WouldBlock` does not merge or lose a partially sent datagram;
- the queue can wrap without splitting a block.

The cost is extra complexity in the capacity check and the extra memory region,
but it is necessary for correct UDP behavior.

## 11. Practical debugging notes

- A warning `Client buffer full` on the TCP path is normal under a fast download
  and a slow adb link; the connection is intended to recover via the pending
  packet source. That path has a sequence-number caveat documented in
  [05 §7.4](05-tcp-proxy.md#74-client-buffer-full-deferred-packet).
- A warning `Cannot send to client, drop packet` on UDP means a response datagram
  was lost because the tunnel was full. Applications with their own retries
  (DNS, QUIC, game protocols) will usually recover.
- A warning `Datagram buffer is full` means the device sent UDP faster than the
  host socket could send; datagrams were intentionally dropped.
- `WouldBlock` is not an error. It is the normal signal to stop draining and wait
  for the next readiness event.
- `IDLE_TIMEOUT_SECONDS` is measured from any event, not just successful traffic;
  a socket that keeps producing readiness events will not expire.

Continue with [07 — Lifecycle, errors, and diagnostics](07-lifecycle-errors-and-diagnostics.md).
