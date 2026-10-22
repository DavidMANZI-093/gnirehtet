# 03 — Tunnel server, selector, and client

[← Previous: CLI and control plane](02-cli-and-execution.md) · [Index](README.md) · [Next: packet model →](04-packet-model.md)

This chapter follows the data plane from "adb connects to the relay" to "the
packet is in the router." It explains the readiness loop, the `TunnelServer`
listener, the `Client` object, the client-id handshake, stream framing, the
outgoing buffer path, and the `Router` that owns per-flow connections.

The actual TCP and UDP engines are covered in [05](05-tcp-proxy.md) and
[06](06-udp-and-buffering.md). The packet codec is covered in
[04](04-packet-model.md).

## 1. The top-level relay loop

`relay::run(port)` creates a `Relay` and calls `run`:

```rust
pub fn run(port: u16) -> io::Result<()> {
    Relay::new(port).run()
}

impl Relay {
    pub fn run(&self) -> io::Result<()> {
        let mut selector = Selector::create()?;
        let tunnel_server = TunnelServer::create(self.port, &mut selector)?;
        info!(target: "Relay", "Relay server started");
        self.poll_loop(&mut selector, &tunnel_server)
    }
}
```

The listener is created before the "started" log message, so a bind failure
prevents the message and propagates to `main`.

### 1.1 Poll loop

`Relay::poll_loop` uses a 1024-event buffer and a cleanup deadline:

```text
next_cleaning_deadline = now + 120 s

loop:
    timeout = max(deadline - now, 0)
    poll(events, timeout)        // retry on EINTR
    now = Instant::now()

    if now >= deadline:
        tunnel_server.clean_up(selector)
        deadline = now + 60 s
    else if events.is_empty():
        debug!("Spurious wakeup")
        continue

    run_handlers(events)
```

Important details:

- The initial deadline is `IDLE_TIMEOUT_SECONDS` (120 s), so no UDP flow can be
  expired before it has had a chance to live that long.
- After the first cleanup, the interval is 60 s. The cleanup is a full scan of
  every client's connections.
- `retry_on_intr!` retries `poll` if the OS returns `EINTR`. This matters on Unix
  when signals (including Ctrl+C handling in the control plane) interrupt the
  syscall.
- If the poll deadline has passed, cleanup runs and the loop proceeds to process
  any events that were returned in the same iteration. It does not `continue`.
- If no events arrived and the deadline has not passed, the code logs a spurious
  wakeup and loops again. This is normal when a timeout or signal wakes `poll`.
- There is no shutdown condition. The loop runs until the process exits or an
  error escapes.

The top-level loop itself logs only "Relay server started" at `Info`. Client
connect/disconnect and TCP/UDP open/close also log at `Info` from their modules;
per-event and per-state details are `Debug`. This is why `-v` is the normal
troubleshooting mode.

## 2. Selector: `mio` plus a slab

`Selector` is a small abstraction over `mio::Poll` that avoids repeating
registration and handler bookkeeping in every object.

```rust
pub struct Selector {
    poll: Poll,
    handlers: Slab<Rc<dyn EventHandler>>,
    tokens_to_remove: Vec<Token>,
}
```

### 2.1 Registration

`register`:

1. inserts `Rc::new(handler)` into the slab and uses the slab key as the token;
2. registers the socket source with `mio` and that token;
3. if registration fails, removes the slab entry immediately and returns the
   error.

The handler trait is implemented for any `Fn(&mut Selector, Event)`. In practice
each object builds a closure that captures its `Rc<RefCell<Self>>`:

```rust
let rc2 = rc.clone();
let handler = move |selector: &mut Selector, event| {
    rc2.borrow_mut().on_ready(selector, event);
};
let token = selector.register(&mut self_ref.stream, handler, interests)?;
```

This is why the code contains comments about annotating the selector type: Rust
needs help inferring the closure's argument type in this position.

### 2.2 Lifecycle operations

| Method | Effect |
|--------|--------|
| `register` | Add a handler to the slab and register the source with a new token. |
| `register_existing` | Register a source again with an existing token after `suspend`. |
| `reregister` | Change the interests of an already-registered source. |
| `suspend` | Deregister the source from `mio` but keep the handler in the slab. |
| `deregister` | Deregister the source from `mio` and queue the token for removal. |
| `remove` | Queue a token for removal without touching `mio` (used when the source is already suspended). |

The distinction between `register_existing`/`suspend` and `deregister` matters for
TCP connections that temporarily have no interest (for example, when the client's
window is zero and there is nothing to write). They suspend the source so they
can later re-register without allocating a new token.

### 2.3 Deferred removal

`deregister` and `remove` do not remove the slab entry immediately. They append
the token to `tokens_to_remove`. `run_handlers` processes the entire event batch
and only then calls `clean_removed_tokens`.

Why? A single poll batch can contain events for a socket and for another object
that is closed as a side effect of handling the first event. Removing slab
entries during iteration could invalidate a token or drop an `Rc` that the current
event still needs. Deferring removal makes batch processing safe.

`run_handlers` clones the handler `Rc` from the slab before calling it. The clone
keeps the object alive even if the handler removes its own token during the call.
If the token is not found, the code panics with `Token not found`; this is an
internal invariant failure, not a recoverable condition.

### 2.4 Edge-triggered behavior

`mio` on Linux uses edge-triggered readiness. Every handler must therefore drain
its source until it gets `WouldBlock`. The relay follows this rule:

- `TunnelServer::on_ready` loops accepting until `WouldBlock`.
- `Client::process_receive` reads until `WouldBlock` or EOF.
- `Client::process_send` writes until the buffer is empty or `WouldBlock`.
- TCP `process_send`/`process_receive` loop until `WouldBlock` or a state change.
- UDP `process_send`/`process_receive` loop until `WouldBlock`.

If a handler were to stop at the first successful read/write without draining, it
might not receive another readiness event for data that is already pending.

## 3. TunnelServer: the listener

`TunnelServer` is an `Rc<RefCell<…>>` because it needs a handler and a callback
that can refer back to it.

```rust
pub struct TunnelServer {
    self_weak: Weak<RefCell<TunnelServer>>,
    clients: Vec<Rc<RefCell<Client>>>,
    tcp_listener: TcpListener,
    next_client_id: u32,
}
```

### 3.1 Binding

```rust
let localhost = Ipv4Addr::new(127, 0, 0, 1).into();
let addr = SocketAddr::new(localhost, port);
let server = TcpListener::bind(addr)?;
```

The listener is **loopback-only**. `mio::net::TcpListener::bind` creates a
nonblocking listener. If another process is using the port, the error propagates
to `main`, which logs and exits with code 3.

### 3.2 Accept loop

`on_ready` loops:

```text
accept_client(selector)
  ├─ Ok  → log "New client accepted"
  ├─ WouldBlock → break
  └─ other error → log and break
```

A single readiness event may represent multiple queued connections, so looping is
required.

`accept_client`:

1. Accepts a `TcpStream`.
2. Copies `next_client_id` and increments it.
3. Builds a close listener closure that captures a `Weak<TunnelServer>`. When the
   client closes, the closure upgrades the weak reference, borrows the server, and
   removes the client by pointer identity.
4. Calls `Client::create(id, selector, stream, on_client_closed)`.
5. Pushes the returned `Rc<RefCell<Client>>` into `clients`.
6. Logs `Client #<id> connected`.

`next_client_id` starts at 0 and increments by one per accepted connection. There
is no wrap protection; `u32` overflow would require billions of connections in one
process lifetime. The id is sent to the client and used in logs, not as a routing
key.

### 3.3 Removing a client

`remove_client(client)` finds the client in the vector by pointer equality
(`ptr::eq(client, item.as_ptr())`), logs `Client #<id> disconnected`, and uses
`swap_remove`. If the client is not found, it panics with "Trying to remove an
unknown client"; that indicates a broken close-listener invariant.

The close listener is invoked from `Client::close`. It holds only a `Weak`, so it
does not keep the server alive. The server itself holds strong client references;
removing the client from `clients` drops the last strong reference (unless a
pending packet source or an in-flight handler still holds one).

### 3.4 Periodic cleanup

`TunnelServer::clean_up` simply iterates all clients and calls
`clean_expired_connections` on each. Expiration logic lives in the router and
connection implementations; see [07](07-lifecycle-errors-and-diagnostics.md).

## 4. Client: one accepted device stream

`Client` is the central object for each device connection. It owns:

```text
id                  u32
stream              mio TcpStream
interests           Interest
token               Token
client_to_network   Ipv4PacketBuffer (64 KiB)
network_to_client   StreamBuffer (16 * 64 KiB capacity)
router              Router
close_listener      Box<dyn CloseListener<Client>>
closed              bool
pending_packet_sources  Vec<Rc<RefCell<dyn PacketSource>>>
pending_id_bytes    usize (starts at 4)
```

### 4.1 Creation and registration

`Client::create`:

1. Sets initial interests to `WRITABLE` because the first job is to send the
   client id.
2. Builds an `Rc<RefCell<Client>>` with a placeholder token.
3. Stores a `Weak` back-reference in the router so connections can remove
   themselves.
4. Builds the handler closure, registers the stream with `mio`, and overwrites
   the placeholder token with the real one.
5. Returns the `Rc`.

The client is not registered for `READABLE` initially. It must first send the id,
then it can read packets. This is a protocol-level ordering rule, not just a
performance choice.

### 4.2 The 4-byte client-id handshake

`pending_id_bytes` starts at 4. On writable readiness, `process_send` loops while
`must_send_id()` is true:

```rust
let raw_id = binary::to_byte_array(self.id); // big-endian u32
let w = self.stream.write(&raw_id[4 - self.pending_id_bytes..])?;
self.pending_id_bytes -= w;
```

Only the remaining suffix is written. Partial writes are handled naturally by
subtracting `w`; the next writable event resumes at the correct offset. A write
of zero bytes is converted to `WriteZero` and closes the client. An I/O error is
logged and also closes the client.

Once `pending_id_bytes == 0`, the relay logs `Client id #<id> sent to client` and
proceeds to any buffered payload. The Android client reads these four bytes as a
big-endian integer and treats a successful read as proof that the relay is actually
present and speaking the protocol.

### 4.3 Why the id is needed

`adb reverse` can accept a connection on the device even when no host process is
listening on the forwarded port. Without a handshake, the Android client could
believe it is connected and start sending packets into a dead tunnel. The id is a
cheap positive signal: it is sent only by the relay, immediately after accept.
It is not a security token and does not authenticate the relay.

## 5. Incoming path: device bytes to router packets

`Client::process_receive` runs on readable readiness:

```rust
loop {
    if self.client_to_network.remaining() == 0 {
        break;
    }
    match self.read() {
        Ok(true)  => self.push_to_network(selector),
        Ok(false) => { /* EOF */ self.close(selector); break; }
        Err(WouldBlock) => break,
        Err(err) => { /* log */ self.close(selector); break; }
    }
}
```

### 5.1 Reading

`Client::read` calls `Ipv4PacketBuffer::read_from(&mut self.stream)`, which
delegates to `ByteBuffer::read_from`. That reads into `buf[head..]` and advances
`head` by the number of bytes read. It returns `Ok(false)` on zero bytes (EOF).

The incoming buffer is exactly `MAX_PACKET_LENGTH = 1 << 16 = 65536` bytes. This
is one more than the largest possible IPv4 total length (65535), so a complete
maximum-size packet always fits.

### 5.2 Recovering packets

`push_to_network` loops:

```rust
while self.push_one_packet_to_network(selector) {
    self.client_to_network.next();
}
```

`push_one_packet_to_network` asks the buffer for an `Ipv4Packet`:

- `Ipv4PacketBuffer::as_ipv4_packet` first checks whether at least 4 bytes are
  available and whether the declared total length is present.
- It asserts the version nibble is 4. A non-IPv4 stream is a hard protocol
  violation and panics; the Android client is expected to filter those packets.
- On success, `Ipv4Packet::parse` creates a view over the complete packet and
  parses IPv4 plus (if recognized) TCP/UDP.

`Ipv4PacketBuffer::next` recomputes the available length and consumes exactly
that many bytes from the front of the `ByteBuffer`. The remaining bytes (the next
packet) are shifted to the front by `ByteBuffer::consume`.

The relay therefore does **not** require the Android client to align writes with
packet boundaries. One read may contain several packets, one packet, or part of a
packet; the buffer waits until the declared length is complete.

### 5.3 Passing to the router

Inside `push_one_packet_to_network`, the client builds a `ClientChannel` (the
borrowed view described below) and calls:

```rust
self.router.send_to_network(selector, &mut client_channel, packet);
```

The router either finds an existing flow or creates a new TCP/UDP connection. The
packet itself is not queued in the client after this call; `next()` consumes it.
If the connection needs to send a response immediately (for example, a SYN-ACK),
it writes through the `ClientChannel` into `network_to_client` before the incoming
packet is consumed.

### 5.4 Buffer-full behavior on input

The read loop stops if `client_to_network.remaining() == 0`. With a 64 KiB
buffer and a maximum packet length of 65535, a complete packet should fit. If the
buffer contains a partial packet and is full, that would require a declared length
larger than the buffer, which the parser would not accept as complete; in normal
traffic this does not happen. If it does, the client stops reading and the
connection remains readable; this is an invariant violation rather than a
supported recovery mode.

## 6. Outgoing path: response packets to the device

The response path exists in two variants because a connection can send a packet
while the client is either borrowed or not.

### 6.1 `Client::send_to_client`

```rust
if packet.length() as usize <= self.network_to_client.remaining() {
    self.network_to_client.read_from(packet.raw());
    self.update_interests(selector);
    Ok(())
} else {
    warn!("Client buffer full");
    Err(WouldBlock)
}
```

`network_to_client` is a `StreamBuffer` with capacity `16 * MAX_PACKET_LENGTH`
= 1,048,576 bytes. It stores raw packet bytes without boundaries; the Android
client re-frames them.

If the buffer has room, the packet is copied and interests are updated to
`READABLE | WRITABLE` (readable because the client still wants incoming device
data). If not, the call returns `WouldBlock` and the caller decides what to do.

### 6.2 `ClientChannel`

`ClientChannel` holds:

```rust
network_to_client: &mut StreamBuffer,
stream: &mut TcpStream,
token: Token,
interests: &mut Interest,
```

Its `send_to_client` is functionally the same as `Client::send_to_client`, but it
does not require borrowing the whole client. The channel's `update_interests`
re-registers the stream when the buffer transitions between empty and non-empty.

Use cases:

- A connection handling a packet inside `Router::send_to_network` is already
  inside a mutable borrow of the client. It must use `ClientChannel`.
- A connection handling a socket readiness event (`TcpConnection::on_ready`) is
  not inside a client borrow. It can use `Client::send_to_client` after upgrading
  its weak client reference.

The code mirrors this distinction with `reply_empty_packet_to_client` (takes a
`ClientChannel`) versus `send_empty_packet_to_client` (borrows the client and
creates a temporary channel).

### 6.3 Writing to the socket

`Client::process_send` first sends the id, then loops:

```text
send_id()
loop:
    write_to(stream)         // StreamBuffer writes one contiguous segment
    if no pending sources:
        break
    process_pending()
    if network_to_client is empty:
        break
```

Calling `write_to` when the buffer is already empty is harmless; the important
part is that pending packet sources are polled after the buffer drains.

`StreamBuffer::write_to` writes at most the contiguous region from `tail` to
`head` (or to the end of the circular buffer), updates `tail`, and resets head and
tail to zero when the buffer becomes empty. A return of `0` from the underlying
write is treated as `WriteZero` and closes the client.

If a write would block, `process_send` returns and leaves the buffer intact. The
interest still contains `WRITABLE`, so `mio` will call again when the socket is
writable. (`Client::process` then recomputes the full interest mask.)

### 6.4 Deferred TCP packets

TCP response packets are built before they are enqueued. If the client buffer is
full, throwing the packet away would desynchronize TCP sequence numbers: the
connection has already advanced its packetizer state, and the device expects that
segment. Instead:

1. `TcpConnection::process_receive` records the packet length in
   `packet_for_client_length` and registers itself as a pending packet source on
   the client.
2. It stops reading from the host socket (`may_read` returns false while a packet
   is pending).
3. When the client buffer drains, `Client::process_pending` pulls the packet via
   `PacketSource::get`, which calls `Packetizer::inflate(len)` on the retained
   buffer.
4. If enqueue succeeds, `PacketSource::next` advances the TCP sequence number and
   clears the pending length. On this deferred path the stored value is the whole
   IPv4 packet length, not the TCP payload length; see the note in
   [05 §7.4](05-tcp-proxy.md#74-client-buffer-full-deferred-packet).
5. If enqueue still fails, the source is put back into the pending vector.

This is the only backpressure mechanism on the response path. UDP has no analogue:
if the client buffer is full, the UDP packet is simply dropped and logged.

## 7. Router: flow lookup and connection ownership

`Router` maps a flow to a `Connection`:

```rust
pub struct Router {
    client: Weak<RefCell<Client>>,
    connections: Vec<Rc<RefCell<dyn Connection>>>,
}
```

The vector is used instead of a `HashMap` because the expected number of
simultaneous connections per device is small. Lookup is linear, which keeps the
code simple and avoids hashing overhead in the common case.

### 7.1 Connection id

`ConnectionId` contains:

```text
protocol:        Tcp | Udp
source_ip:       u32 (big-endian numeric form)
source_port:     u16
destination_ip:  u32
destination_port:u16
id_string:       "source:port -> destination:port"
```

It is constructed from parsed headers via `from_headers`. Equality is derived;
the router compares whole ids with a linear scan. The `id_string` is for logs.

The id is directional. Device-originated packets always have the device as source
and the remote endpoint as destination. The relay does not need to look up
responses by flow because it generates them from the connection's saved
packetizer; it never parses a response from the host as a device packet.

### 7.2 The 10.0.2.2 rewrite

`ConnectionId::rewritten_destination` maps:

```text
10.0.2.2 → 127.0.0.1
```

and leaves every other destination unchanged. `10.0.2.2` is the Android
emulator's alias for the host loopback. The real host socket must connect to
`127.0.0.1`; the forged response still uses `10.0.2.2` as its source because the
packetizer reversed the original header.

This rewrite applies to both TCP and UDP.

### 7.3 Finding or creating a connection

```rust
let (ipv4_header_data, transport_header_data) = ipv4_packet.headers_data();
let transport_header_data = transport_header_data.expect("No transport");
let id = ConnectionId::from_headers(ipv4_header_data, transport_header_data);
let index = match self.find_index(&id) {
    Some(index) => index,
    None => {
        let connection = Self::create_connection(selector, id, self.client.clone(), ipv4_packet)?;
        let index = self.connections.len();
        self.connections.push(connection);
        index
    }
};
```

`create_connection` dispatches on protocol:

- `Tcp` → `TcpConnection::create`
- `Udp` → `UdpConnection::create`
- anything else → `io::Error::other("Unsupported protocol")`

In practice `ConnectionId` is only built for valid packets, and
`Ipv4Packet::is_valid()` is false for other protocols because
`TransportHeaderData::parse` returns `None`. Such packets are dropped before the
router attempts connection creation.

### 7.4 Sending a packet to a connection

`Router::send_to_network`:

1. If the packet is not valid (no TCP/UDP transport), log a warning and, if
   `Trace` is enabled, dump the first bytes. Return.
2. Find or create the connection.
3. Borrow the connection and call `send_to_network(selector, client_channel, packet)`.
4. If the connection reports `is_closed()`, remove it with `swap_remove`.
5. If connection creation fails, log and drop the packet.

The closed check after the call is important: handling a packet can close a
connection (RST, write failure, final FIN ACK), and the router must not retain a
dead object.

### 7.5 Connection removal

There are two removal paths:

- **Router-initiated:** after `send_to_network`, if the connection is closed, the
  router swaps it out immediately.
- **Self-initiated:** when handling a socket readiness event, a connection that
  closes calls `remove_from_router` on itself, because `on_ready` is not called
  through the router. This upgrades the weak client reference, borrows the client,
  and calls `Router::remove`.

`Router::remove` finds the connection by thin-pointer identity. It uses
`binary::ptr_data_eq`, which compares only the data pointer of possibly fat
pointers and ignores the vtable. The comment records a historical issue where
the vtable portion could differ for the same object; comparing the data pointer
avoids that. If no match is found, it panics ("Removing an unknown connection").

`Router::clear` closes every connection and clears the vector; it is called when
the client closes.

### 7.6 Periodic expiration

`Router::clean_expired_connections` iterates **in reverse**:

```rust
for i in (0..self.connections.len()).rev() {
    let expired = { ... connection.is_expired() ... };
    if expired {
        self.connections.swap_remove(i);
    }
}
```

Reverse iteration matters because `swap_remove(i)` moves the last element into
slot `i`; iterating forward could skip that moved element or index out of bounds.
TCP always returns `false` for `is_expired`; UDP uses a 120-second idle timeout.

## 8. Client close and connection close

### 8.1 Client close

`Client::close`:

```rust
self.closed = true;
selector.deregister(&mut self.stream, self.token).unwrap();
self.stream.shutdown(Shutdown::Both) ...;
self.router.clear(selector);
self.close_listener.on_closed(self);
```

Notes:

- The socket is not explicitly dropped here; it is closed when the `Client` is
  dropped. `shutdown` asks the OS to close both directions immediately.
- `router.clear` closes every TCP/UDP connection (deregistering their sources).
- The close listener removes the client from `TunnelServer::clients`.
- The `unwrap` on `deregister` reflects the assumption that the stream is still
  registered. If it is not, this is an internal error and panics.

`Client::process` calls `close` when:

- the stream reports EOF (`read` returned `Ok(false)`);
- a read or write fails with an error other than `WouldBlock`;
- the event has neither readable nor writable flags (error/hup);
- writing the client id fails.

Once `closed` is true, subsequent `on_ready` calls should not occur because the
source is deregistered; the `if !self.closed` guard is defensive.

### 8.2 Connection close

Connections close on protocol errors, EOF, RST, final FIN acknowledgment, or
(for UDP) idle expiration. Their `close` implementations deregister or
remove their event token and set `closed = true`; the socket is dropped by RAII.
The router removes them either immediately (in `send_to_network`) or via
self-removal (`on_ready`). This avoids double removal because exactly one of the
two paths runs for a given close cause.

## 9. Interest management

Interest transitions are the heartbeat of the nonblocking design. A mistake can
cause a stall (never notified despite pending work) or a busy loop (notified
forever with nothing to do).

### 9.1 Client interests

`ClientChannel::update_interests` (also used by `Client::update_interests`):

```text
network_to_client empty      → READABLE
network_to_client non-empty  → READABLE | WRITABLE
```

The client always wants to read from the device. It wants to write only when it
has response bytes. The initial state is `WRITABLE` alone, before the id is sent;
after the id is sent (and after any enqueue), this function moves it to
`READABLE` or `READABLE | WRITABLE`.

### 9.2 TCP interests

`TcpConnection::update_interests`:

```text
state == SynSent                  → WRITABLE
otherwise:
    may_read()  → READABLE
    may_write() → WRITABLE (OR with readable)
    neither     → None
```

When the desired interest is `None`, the source is suspended (deregistered but
handler retained). When it transitions from `None` to `Some`, the code uses
`register_existing`; when both are `Some`, it uses `reregister`. The connection
logs the interest at `Debug`.

`may_read` is more subtle:

```text
not connected / closed             → false
a packet is pending for the client → false
remaining_client_window <= 0       → false
otherwise                          → true
```

`may_write` is simply "the `client_to_network` stream buffer is not empty."

### 9.3 UDP interests

```text
client_to_network empty      → READABLE
client_to_network non-empty  → READABLE | WRITABLE
```

A connected UDP socket is readable whenever datagrams arrive; writable is added
when there are queued datagrams that could not be sent yet (for example, after a
`WouldBlock`).

## 10. Client lifecycle in one table

| Phase | Trigger | What happens |
|-------|---------|--------------|
| Accepted | `TunnelServer::accept_client` | `Client::create`, registered as `WRITABLE`, id pending |
| Handshake | Writable | Write 4-byte big-endian id; partial writes handled |
| Relaying | Writable/readable | Send buffered packets; read IPv4 packets and route them |
| Backpressure | Client buffer full | TCP connection becomes a pending packet source; UDP drops |
| EOF/error | Read returns 0 or error | `close`: deregister, shutdown, close connections, notify server |
| Server removal | Close listener | Remove `Rc<Client>` from `TunnelServer::clients` |

The next chapter explains exactly what happens inside `Router::send_to_network`
when a packet is parsed: [04 — Packet model and forging](04-packet-model.md).
