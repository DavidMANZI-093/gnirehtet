# 05 — TCP proxy

[← Previous: packet model](04-packet-model.md) · [Index](README.md) · [Next: UDP and buffers →](06-udp-and-buffering.md)

TCP is the most intricate part of the relay. The Android device believes it is
talking to a remote server, but the relay is actually the other endpoint: it
terminates the device-side TCP connection, opens its own host TCP connection to
the destination, and proxies payload between the two. To keep the device's TCP
stack happy, it must forge SYN-ACK, ACK, PSH, FIN, and RST packets with coherent
sequence numbers.

This chapter explains the `TcpConnection` implementation, its control block, the
state machine, both data directions, flow control, and the edge cases that fall
out of this design.

## 1. Why a TCP state machine is necessary

A naive proxy might simply copy bytes from the device's stream to a host socket.
That does not work because the device is not sending a byte stream to the relay;
it is sending **TCP segments** addressed to the remote server. The device expects:

- a SYN-ACK proving the remote endpoint accepted the connection;
- acknowledgments for bytes it sent;
- data segments with the remote server's sequence numbers;
- a FIN or RST when the connection ends.

The host kernel will not generate those packets for the device: the host socket's
TCP connection is between the host and the remote server, not between the device
and the remote server. The relay therefore has to generate the device-facing side
of the TCP conversation itself.

The relay's TCP engine is intentionally **not** a full TCP stack. It does not
retransmit, implement congestion control, negotiate options, or reassemble
fragments. It maintains just enough state to:

1. accept the device's SYN;
2. complete the handshake with a forged SYN-ACK;
3. map device sequence numbers to host stream bytes;
4. map host stream bytes back to device sequence numbers;
5. close both directions coherently.

## 2. The control block

`TcpConnection` holds:

```rust
pub struct TcpConnection {
    self_weak: Weak<RefCell<TcpConnection>>,
    id: ConnectionId,
    client: Weak<RefCell<Client>>,
    stream: TcpStream,
    interests: Option<Interest>,
    token: Token,
    client_to_network: StreamBuffer,
    network_to_client: Packetizer,
    packet_for_client_length: Option<u16>,
    closed: bool,
    tcb: Tcb,
}
```

| Field | Role |
|-------|------|
| `self_weak` | Lets the connection register itself as a pending packet source without a cycle. |
| `id` | Flow key and log prefix. |
| `client` | Weak reference to the tunnel client; used to enqueue packets and remove from router. |
| `stream` | Real nonblocking host TCP socket to the destination. |
| `interests` | Current `mio` interest, or `None` when suspended. |
| `token` | Event-loop token for the host stream. |
| `client_to_network` | Bytes received from the device and waiting to be written to the host socket. |
| `network_to_client` | Packetizer that builds response packets from host reads. |
| `packet_for_client_length` | Length of a built response packet that could not be enqueued yet. |
| `closed` | Set on any terminal path; used by the router. |
| `tcb` | Transmission control block. |

### 2.1 TCB fields

```rust
struct Tcb {
    state: TcpState,
    syn_sequence_number: u32,           // device's initial sequence number
    sequence_number: Wrapping<u32>,     // next server byte/segment sequence
    acknowledgement_number: Wrapping<u32>, // next expected device byte
    their_acknowledgement_number: u32,  // last ack received from device
    fin_sequence_number: Option<u32>,   // sequence occupied by our FIN
    fin_received: bool,                 // device sent FIN
    client_window: u16,                 // most recent device-advertised window
}
```

Sequence arithmetic uses `Wrapping<u32>` for additions and subtractions. TCP
sequence numbers wrap at 2³²; `Wrapping` makes the carry behavior explicit. The
code does not attempt to handle delayed duplicates or PAWS (timestamp) semantics;
it assumes a local, well-behaved device.

### 2.2 States

```text
Init
SynSent         // device SYN recorded, host connect in progress
SynReceived     // host connected, SYN-ACK sent, waiting for device ACK
Established     // handshake complete
CloseWait       // declared but not reached in this implementation
LastAck         // we sent FIN, waiting for ACK of FIN
Closing         // simultaneous close, waiting for ACK of FIN
FinWait1        // we sent FIN, waiting for device FIN
FinWait2        // we sent FIN, device FIN received, waiting for ACK
```

`TcpState::is_connected` is true for `Established`, `CloseWait`, `LastAck`,
`Closing`, `FinWait1`, and `FinWait2`. `is_closed` is true for `LastAck`,
`Closing`, `FinWait1`, and `FinWait2`. Note the slightly surprising naming: these
are states in which the proxy is shutting down, not necessarily states where the
object has already been dropped.

`CloseWait` is present for RFC completeness but is not used: when the device
sends a FIN in `Established`, the relay immediately sends its own FIN and moves to
`LastAck`, bypassing `CloseWait`. The code comment says this is deliberate because
the connection will be closed by RAII.

## 3. Connection creation

`TcpConnection::create` is called by `Router` when the first packet for a TCP
flow arrives. It:

1. Logs `Open` with the flow id.
2. Calls `TcpStream::connect(id.rewritten_destination())`. `mio` creates a
   nonblocking socket.
3. Extracts the TCP header from the first packet.
4. Copies the first 20 bytes of the TCP header into a stack buffer, shrinks the
   data offset to 5, and asserts the resulting header length is 20.
5. Creates a `Packetizer` from the IPv4 header and the shrunk TCP header.
6. Sets initial interests to `WRITABLE` (needed to detect connect completion).
7. Builds the `Rc<RefCell<TcpConnection>>`, stores a self-weak, registers the
   stream with a handler closure, and saves the token.

The reference header is taken from the **first** packet, which is normally a SYN
with options. Shrinking options before packetizing means every response has a
20-byte TCP header and no options. This simplifies checksumming and avoids having
to synthesize option values.

If `TcpStream::connect` fails immediately (for example, an invalid address or a
local resource error), `create` returns an `io::Error`, the router logs
`Cannot create route, dropping packet`, and no connection is created. The device
will retransmit its SYN; a later attempt may succeed.

## 4. Readiness processing

`TcpConnection::on_ready` calls `process` and treats `WouldBlock` as a spurious
event. Any other error panics with `Unexpected unhandled error`; internal code
normally converts socket errors into close/RST paths before they reach this point.

`process`:

```text
if closed: return
if readable or writable:
    if writable:
        if state == SynSent: process_connect()
        else if may_write(): process_send()
    if not closed and readable and may_read(): process_receive()
    if not closed: update_interests()
else:
    close()                        // error or hup
if closed: remove_from_router()
```

Ordering matters:

- Writable is handled before readable. In `SynSent`, this is how the host
  connection completion is detected.
- `process_connect` is called even if `may_write` is false. The write interest in
  `SynSent` exists solely to wait for connectability.
- Read and write processing are guarded by state and interest conditions, so a
  single event carrying both flags is safe.
- If the connection closed during either phase, the second phase is skipped.
- `remove_from_router` is only called on the `on_ready` path, because a close
  caused by `Router::send_to_network` is handled by the router's post-call check.

### 4.1 Detecting a failed host connect

The code does not call `take_error()` when the `SynSent` socket becomes writable.
It assumes writable readiness means the connect completed and sends a forged
SYN-ACK. If the connect actually failed, the failure is discovered when the stream
is next read or written (or when an error/hup event is delivered) after the
handshake; the connection then closes, sending a RST where the code path allows.
If the device never completes the handshake, the flow can remain half-open. This
is a deliberate simplification: the device may see a short-lived connection
rather than an immediate refusal. It also means a host connect failure is not
reported through a dedicated error path.

## 5. Handshake

### 5.1 The device SYN

The first packet for a flow reaches `handle_packet` while the TCB is in `Init`.
It calls `handle_first_packet`:

```text
if packet has SYN:
    their_seq = tcp.sequence_number
    ack_number = their_seq + 1
    syn_sequence_number = their_seq
    sequence_number = random u32 from getrandom
    client_window = tcp.window
    state = SynSent
else:
    sequence_number = tcp.acknowledgement_number
    forge RST to client
    close
```

The random server ISN comes from `getrandom::u32()`. If the OS RNG fails, the
code panics; this is treated as an unrecoverable environment problem. The client's
ISN is `syn_sequence_number`; it is used to recognize duplicate SYNs later.

A non-SYN first packet is rejected with a RST whose sequence number is the
client's acknowledgment number, placing the reset in the device's receive window.
This is the conventional response to an unexpected segment.

### 5.2 Host connection and SYN-ACK

When the host stream becomes writable and the state is `SynSent`,
`process_connect`:

1. Asserts the state is `SynSent`.
2. Sets state to `SynReceived`.
3. Sends an empty packet with `SYN | ACK` using the current TCB sequence and
   acknowledgment numbers.
4. Increments `sequence_number` by one because SYN consumes one sequence number.

The forged SYN-ACK therefore has:

```text
seq = random server ISN
ack = client ISN + 1
flags = SYN | ACK
```

### 5.3 The device ACK

The next device packet normally has `ACK`, sequence `client ISN + 1`, and
acknowledgment `server ISN + 1`. `handle_packet`:

- verifies the sequence number against `acknowledgement_number + buffered bytes`;
- records the device window and acknowledgment number;
- calls `handle_ack`.

`handle_ack` checks the state: if it is `SynReceived`, it sets `Established` and
returns. That completes the handshake.

One subtlety: because `handle_ack` returns immediately for the handshake ACK, any
payload piggybacked on that exact packet is not copied to `client_to_network`.
In practice the handshake ACK is empty; this is a limitation worth remembering if
you are debugging unusual clients.

### 5.4 Duplicate SYNs

`handle_packet` intercepts any SYN and calls `handle_duplicate_syn`:

- If still in `SynSent` (host connect not yet complete), the relay accepts the
  retransmitted SYN as if it were the first: it updates `syn_sequence_number` and
  `acknowledgement_number`, but keeps the random server sequence number. The
  pending `process_connect` will send the SYN-ACK.
- If the state is past `SynSent` and the SYN's sequence number equals
  `syn_sequence_number`, it is a duplicate; the code silently ignores it.
- If the sequence number differs, it is a different connection attempt reusing
  the same tuple; the relay sends RST and closes.

The duplicate-SYN path handles retransmissions that arrive before
`process_connect` sends the SYN-ACK. Once the SYN-ACK has been sent, a duplicate
with the same ISN is ignored; there is no SYN-ACK retransmission.

## 6. Device → host data path

### 6.1 Sequence validation

For every non-SYN packet after `Init`, `handle_packet` computes:

```text
expected = acknowledgement_number + client_to_network.size()
```

`acknowledgement_number` is the next device byte the relay expects. The stream
buffer holds bytes already received but not yet written to the host socket. Thus
`expected` is the sequence number after all accepted-but-unwritten device data.

If the packet's sequence number differs, the relay logs a warning and ignores the
packet. This handles:

- retransmissions of data already accepted;
- out-of-order segments;
- duplicates generated by the device stack.

The relay does not queue out-of-order data or generate duplicate ACKs. It relies
on the device TCP stack to retransmit when needed. This is a significant design
choice: the relay is not a reliable endpoint in the RFC sense; it is a proxy that
accepts only in-order segments.

### 6.2 Window and acknowledgment bookkeeping

Before dispatching flags, `handle_packet` records:

```text
client_window = tcp.window
their_acknowledgement_number = tcp.acknowledgement_number
```

`client_window` is used to decide how much data the relay may read from the host
socket. `their_acknowledgement_number` is the device's latest cumulative
acknowledgment of server data.

### 6.3 ACK and payload

`handle_ack` does two different things depending on state:

- In `SynReceived`, it transitions to `Established` and returns (handshake ACK).
- Otherwise, it processes payload:

```text
payload = packet.payload()
if payload is empty: return
if client_to_network.remaining() < payload.len():
    warn "Not enough space, dropping packet"
    return
client_to_network.read_from(payload)
```

The data is **not** acknowledged at this point. The relay delays its ACK until the
bytes are actually written to the host socket. This avoids telling the device that
data was accepted if the host connection subsequently fails.

If the stream buffer is full, the packet is dropped. Because
`acknowledgement_number` and the buffer size do not advance, the expected sequence
number stays the same, so the device's retransmission of the same segment will be
accepted once space is available.

### 6.4 Writing to the host socket

`process_send` runs when the host socket is writable and `client_to_network` is
non-empty:

```text
while buffer non-empty:
    write_to(stream)
    Ok(0):
        log error
        send RST to device
        close
    Ok(w):
        acknowledgement_number += w
        if fin_received and buffer empty:
            do_handle_fin()
            break
        forge ACK
    WouldBlock:
        break
    other error:
        log
        send RST
        close
        break
```

For each successful write chunk, the relay advances `acknowledgement_number` and
forges an ACK to the device, except when the last chunk makes a previously
received FIN ready: in that case it sends `FIN | ACK` instead (see below).
`StreamBuffer::write_to` may write a partial contiguous region, so there can be
multiple ACKs for a large payload. This is valid TCP; the device sees a cumulative
acknowledgment.

If the device had sent a FIN while data was still buffered, `fin_received` is true
and the FIN is deferred until the buffer drains. The FIN consumes one sequence
number and is acknowledged separately (see below).

### 6.5 Zero-byte writes and errors

A zero-byte write from a non-empty stream buffer is treated as fatal; the relay
logs an error, sends RST, and closes. Other write errors also send RST and close.
The `send_empty_packet_to_client` call for RST may itself fail if the client
buffer is full; it is logged as a warning and the connection is closed anyway.

## 7. Host → device data path

### 7.1 Permission to read

`may_read` returns false unless all of the following hold:

- the state is connected (not `Init`/`SynSent`/`SynReceived`);
- the state is not a closing state (`LastAck`, `Closing`, `FinWait1`, `FinWait2`);
- no response packet is pending for the client;
- `remaining_client_window() > 0`.

This single method implements three constraints: state, deferred-packet
backpressure, and device flow control.

### 7.2 Remaining client window

```rust
let wrapped_remaining =
    Wrapping(their_acknowledgement_number)
    + Wrapping(client_window)
    - sequence_number;
let remaining = wrapped_remaining.0;
if remaining <= u32::from(client_window) {
    remaining as u16
} else {
    0
}
```

In ordinary arithmetic, the number of bytes the relay may still send is:

```text
last_ack_from_device + device_window - next_server_sequence
```

If this is negative (the relay has already sent beyond the current window), the
wrapped unsigned result is huge; the `remaining <= client_window` guard catches
that and returns 0. This also handles sequence wrap-around.

The relay does not maintain a separate receive window or apply send-window
scaling. `client_window` is the raw 16-bit field from the device's TCP header.
Because the relay strips TCP options, it never negotiates window scaling; it
treats the field as an unscaled byte count, both for its own read gating and in
the reference window copied into forged headers.

### 7.3 Reading and packetizing

`process_receive` loops while `may_read`:

```text
remaining = min(remaining_client_window, MAX_PAYLOAD_LENGTH)
update_headers(packetizer, tcb, ACK | PSH)
packetize_read(host_stream, max_chunk=remaining)
```

`MAX_PAYLOAD_LENGTH` is 16344, matching the Android client's MTU. `update_headers`
sets the response TCP header's:

- sequence number = current server sequence;
- acknowledgment number = next expected device byte;
- flags = `ACK | PSH`.

The TCP window field is **not** updated; it remains whatever was copied from the
reference header. See the limitations section.

On a successful read and enqueue:

- `len = packet.payload().len()`;
- `packet_length = packet.length()`;
- `send_to_client`; if successful, `sequence_number += len`.

The IP total length and TCP checksum were already set by `Packetizer::build`.

### 7.4 Client buffer full: deferred packet

If `send_to_client` returns `WouldBlock` (the client's `network_to_client` is
full):

1. Upgrade the weak client reference.
2. Register this TCP connection as a `PacketSource` on the client.
3. Store `packet_length` in `packet_for_client_length`.
4. Break out of the read loop.

The packetizer's buffer still contains the built packet. `may_read` now returns
false because a packet is pending, so the relay stops reading from the host
socket. This applies backpressure to the remote server through the host TCP
window.

When the client buffer drains, `Client::process_pending` calls
`PacketSource::get`, which returns `Packetizer::inflate(packet_length)`. If the
packet fits, it is enqueued and `PacketSource::next`:

- advances `sequence_number` by the stored value;
- clears `packet_for_client_length`;
- updates interests if not closed.

If it still does not fit, the source is kept in the pending vector.

There is a subtle implementation inconsistency here. In the normal successful
path (§7.3), the relay advances `sequence_number` by the TCP **payload** length.
In the deferred path, `process_receive` stores `ipv4_packet.length()` (the whole
IPv4 packet length) in `packet_for_client_length`, and `PacketSource::next` adds
that same value to the sequence number. A normal response therefore advances the
sequence by an extra 40 bytes (20-byte IP header + 20-byte TCP header). The
pending mechanism is only used when the client buffer is full, but a deferred
packet can desynchronize the device's sequence space. This path is not covered by
the current tests; treat it as a known edge-case bug rather than intended
behavior.

This is the only path that defers a response packet. Control packets (SYN-ACK,
ACK, FIN, RST) do not use the pending-source mechanism; if the client buffer is
full when one is forged, it is logged and lost. The comment claims losing an empty
packet will not break the TCP connection, but there is no control-packet retry in
the relay. A lost SYN-ACK is particularly problematic: once the state has advanced
past `SynSent`, a retransmitted SYN with the same ISN is silently ignored, so the
flow can remain stalled until the device or client retries at a higher level.
ACK-only loss has a similar risk because the relay does not retransmit pure ACKs.
The design assumes the adb tunnel is reliable enough that these losses are rare.

### 7.5 Host EOF

When `packetize_read` returns `Ok(None)` (zero-byte read), the host has closed its
send direction. `eof`:

1. Sends an empty `FIN | ACK` to the device.
2. Records `fin_sequence_number = current sequence`.
3. Increments `sequence_number` by one (FIN consumes one).
4. Sets state:
   - `LastAck` if the state was `CloseWait`;
   - otherwise `FinWait1`.

As noted, `CloseWait` is not actually reached, so a host EOF in `Established`
normally goes to `FinWait1`.

### 7.6 Read errors

A read error other than `WouldBlock` logs the error, sends RST to the device, and
closes. If the error is a connection reset from the remote host, the device sees a
RST; if it is a local resource error, the same path applies.

## 8. Device FIN and closing handshake

### 8.1 Receiving a FIN

`handle_fin` sets `fin_received = true`. If `client_to_network` is empty, it calls
`do_handle_fin` immediately; otherwise it waits until pending device data has been
written to the host socket.

This ordering matters: the FIN is after all data in the device's sequence space,
so the relay must not acknowledge it before delivering the preceding bytes to the
host.

### 8.2 `do_handle_fin`

`do_handle_fin` first increments `acknowledgement_number` by one because the FIN
occupies one sequence number. Then:

| Current state | Action | New state |
|---------------|--------|-----------|
| `Established` | Send `FIN | ACK`; record `fin_sequence_number`; `sequence_number += 1` | `LastAck` |
| `FinWait1` | Send `ACK` | `Closing` |
| `FinWait2` | Send `ACK`; close | closed |
| anything else | Log a warning | unchanged |

The `Established` case bypasses `CloseWait` because there is no application-level
half-close to preserve; the connection is closed by RAII once the final ACK
arrives.

### 8.3 ACK of our FIN

`handle_packet` checks after FIN handling:

```rust
if let Some(fin_seq) = fin_sequence_number
    && tcp.acknowledgement_number() == fin_seq + 1
{
    handle_fin_ack(selector);
}
```

`handle_fin_ack`:

- `LastAck` or `Closing` → close;
- `FinWait1` → `FinWait2`;
- `FinWait2` → ignore (already waiting for the device's FIN);
- anything else → warning.

If the device sends FIN and ACK of our FIN in one packet, `handle_fin` transitions
to `Closing`, and then the FIN-ACK check can close immediately. The code handles
this because the FIN-ACK check is after `handle_fin`.

### 8.4 RST

If an accepted packet has `RST`, `handle_packet` closes immediately. No response
is sent; the device initiated the reset.

## 9. Interest management

`update_interests` is called after packet handling and after readiness processing,
unless the connection has closed.

```text
if state == SynSent:
    interest = Some(WRITABLE)
else:
    interest = None
    if may_read():  interest = Some(READABLE)
    if may_write(): interest = interest | WRITABLE
```

Transitions:

- `Some → Some` changed: `selector.reregister`.
- `None → Some`: `selector.register_existing` (the source was suspended, not
  removed).
- `Some → None`: `selector.suspend` (deregister but keep the handler in the slab).

The connection logs the chosen interest at `Debug`. If `should be readable` is
true but the source was previously suspended, `register_existing` restores it with
the existing token.

The assertion `assert!(!self.closed)` documents that closed connections must not
update interests; callers check `closed` before calling.

## 10. Close paths

`TcpConnection::close`:

1. Logs `Close`.
2. Sets `closed = true`.
3. If `interests.is_some()`, calls `selector.deregister(&mut stream, token)`.
   Deregistration failure is logged as a warning rather than panicking; the comment
   cites mio issue/behavior where deregistration can fail.
4. If `interests` is `None`, the source is already suspended, so it calls
   `selector.remove(token)` to queue the handler for removal.
5. Sets `interests = None`.
6. Lets the `TcpStream` drop (closing the socket).

Close causes include:

- device RST;
- host read/write error;
- final ACK of our FIN in `LastAck`/`Closing`;
- simultaneous close completion in `FinWait2`;
- non-SYN first packet (after sending RST);
- duplicate SYN with a different ISN (after sending RST);
- client close (`Router::clear` calls `close` on every connection).

There is no TCP idle timeout: `is_expired` always returns false for TCP, so the
router's periodic cleanup never closes a TCP connection.

After `on_ready` processing, if `closed` is true, the connection removes itself
from the router. For closes triggered from `Router::send_to_network`, the router
removes it immediately after `send_to_network` returns.

## 11. Backpressure and flow control summary

| Direction | Producer | Buffer | Full behavior |
|-----------|----------|--------|---------------|
| Device → host | Device TCP stack | `client_to_network` (256 KiB stream buffer) | Drop the segment; device retransmits. |
| Host → device | Host TCP socket | `Packetizer` + client `network_to_client` (1 MiB) | Defer one built packet, stop reading host socket. |
| Device advertise window | Device TCP stack | `client_window` | Stop reading host socket until window opens. |
| Host socket not writable | Host kernel | `client_to_network` | Interest `WRITABLE`; retry on readiness. |
| Client tunnel not writable | adb/TCP | `network_to_client` | Interest `WRITABLE`; retry on readiness. |

The relay does not generate window updates for its own receive buffer; forged
headers keep the reference window field. Its actual backpressure to the remote
server is the host TCP receive window, which shrinks naturally when the relay
stops reading.

## 12. What the implementation deliberately omits

- **Retransmission timers.** No RTO, no retransmission queue, no duplicate ACK
  logic. Relies on the device stack and the host kernel.
- **Congestion control.** The host kernel handles the real connection; the forged
  device-side path has no congestion window.
- **SACK, timestamps, window scaling, MSS negotiation.** TCP options are stripped
  from forged headers.
- **Delayed ACKs / Nagle interactions.** The relay ACKs after writing chunks; it
  does not implement a delayed-ACK timer.
- **Half-close semantics.** `CloseWait` is bypassed; an `Established` connection
  that receives a device FIN immediately sends its own FIN and goes to `LastAck`.
- **Out-of-order reassembly.** Packets whose sequence number is not exactly
  expected are ignored.
- **Incoming checksum validation.** The device's packets are trusted.
- **Failed-connect detection before SYN-ACK.** As noted in §4.1, a failed host
  connect is discovered on first I/O, not via `take_error()` at writable
  readiness.
- **Piggybacked handshake data.** The ACK that completes the handshake is consumed
  for the state transition and returns before copying payload.

These choices keep the implementation small and are reasonable for a trusted,
local, single-user reverse-tethering proxy. They are not sufficient for a general
TCP stack.

## 13. Timeline: a short connection

```text
device                         relay                         remote server
  │                              │                                │
  │ SYN seq=1000                 │                                │
  ├─────────────────────────────►│ create host TcpStream ────────►│
  │                              │                                │ SYN
  │                              │◄───────────────────────────────┤ SYN-ACK
  │                              │ (host becomes writable)        │
  │                              │ process_connect()              │
  │◄──── SYN-ACK seq=5000 ack=1001                                │
  │                              │                                │
  │ ACK seq=1001 ack=5001        │                                │
  ├─────────────────────────────►│ state=Established              │
  │                              │                                │
  │ ACK|PSH seq=1001 data="GET"  │                                │
  ├─────────────────────────────►│ buffer; write to host ────────►│ GET
  │◄──── ACK ack=1004            │                                │
  │                              │◄──────────── HTTP response ────┤
  │                              │ packetize ACK|PSH              │
  │◄──── seq=5001 ack=1004 data  │                                │
  │                              │                                │
  │ FIN seq=1004                 │                                │
  ├─────────────────────────────►│ write pending data; ack FIN    │
  │◄──── FIN|ACK seq=... ack=1005│                                │
  │ ACK ack=fin+1                │                                │
  ├─────────────────────────────►│ close                          │
  │                              │                                │
```

The exact sequence numbers in the diagram are illustrative; the important part is
the direction of ownership: the device's SYN is answered by the relay, while the
real SYN to the remote server is generated independently by the host kernel.

Continue with [06 — UDP proxy and buffers](06-udp-and-buffering.md).
