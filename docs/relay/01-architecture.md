# 01 — Architecture: the big picture

[← Index](README.md) · [Next: CLI and control plane →](02-cli-and-execution.md)

This chapter builds the mental model before the line-level chapters. The goal is
to answer three questions:

1. Where does the relay sit in a reverse-tethering session?
2. What are the moving parts, and which thread owns them?
3. How does a packet actually travel from an Android app to the internet and
   back?

## 1. The problem the relay solves

An Android app creates a socket and writes data. Normally the kernel would send
that data over Wi-Fi or mobile data. With _Gnirehtet_, the Android client
installs a VPN interface and takes over the default route, so the kernel hands
the client **raw IPv4 packets** instead. Those packets have the app's original
addresses and ports; the client does not understand or modify TCP/UDP.

The relay must therefore act as the other end of those packets:

- If the device sends a TCP SYN to `93.184.216.34:443`, the relay must open a
  real TCP connection to `93.184.216.34:443`, complete the device-side handshake
  itself, and then proxy payload in both directions.
- If the device sends a UDP datagram to a DNS server, the relay must send one UDP
  datagram to that server and return one UDP datagram to the device.
- If the host cannot open the socket, the relay must tell the device (RST for
  TCP) or drop the packet (UDP), depending on what the protocol allows.

The relay is therefore a **terminating proxy** at the packet level. It does not
simply forward the device's IP packets onto the host network; that would require
raw sockets and would collide with the host's own TCP stack. Instead it uses
normal host sockets and reconstructs the packets the device expects to see.

## 2. The two planes

It helps to separate the code into a **control plane** and a **data plane**.

### Control plane

The control plane is everything needed to get a session running:

```text
command line
   │
   ├─ install APK if needed (adb install)
   ├─ create reverse tunnel (adb reverse localabstract:gnirehtet tcp:31416)
   ├─ send START intent (adb shell am start …)
   └─ run the relay process
```

It lives in `cli.rs`, `executor.rs`, and `adb_monitor.rs`. It is allowed to
block on `adb` subprocesses and `sleep`; it is not part of the hot path.

### Data plane

The data plane is the relay itself: `relay/relay.rs` and everything it owns. It
runs a readiness-driven event loop and never blocks on a socket. Its job is to
move bytes and maintain per-flow state.

```text
      accepted TCP stream                    real host sockets
              │                                     │
              ▼                                     ▼
       Client / Router  ◄── packet codec ──►  TcpConnection / UdpConnection
              │                                     │
              └────────── Ipv4PacketBuffer ─────────┘
                         StreamBuffer / Packetizer
```

## 3. End-to-end packet journeys

### 3.1 Device → internet (request path)

Let an app connect to `example.com:80`, whose current address is `93.184.216.34`.
The Android VPN has address `10.0.0.2`.

1. The app calls `connect()`/`write()`. The kernel routes the traffic into the
   VPN interface.
2. The Android client reads a complete IPv4 packet from the tun fd and writes
   its bytes to the `LocalSocket`.
3. `adb reverse` carries those bytes to the relay's TCP listener.
4. The relay's `TunnelServer` accepts the connection and creates a `Client`.
5. On first write readiness, `Client` sends its 4-byte client id.
6. `Client::process_receive` reads bytes into `client_to_network`
   (`Ipv4PacketBuffer`).
7. `Ipv4PacketBuffer` recognizes a complete packet from the version and total
   length fields. `Ipv4Packet::parse` parses the IPv4 header and TCP header.
8. `Router` builds a `ConnectionId`:
   `Tcp, 10.0.0.2:54321 -> 93.184.216.34:80`.
9. No connection exists, so `Router` calls `TcpConnection::create`. That creates
   a nonblocking host `TcpStream` to `93.184.216.34:80`.
10. The packet is handed to `TcpConnection::handle_packet`. The first packet is a
    SYN, so the connection records the client's initial sequence number, picks a
    random server sequence number, and enters `SynSent`.
11. When the host TCP connection becomes writable, `process_connect` sends a
    forged SYN-ACK to the device and enters `SynReceived`.
12. The device replies with ACK; `handle_ack` enters `Established`.
13. Subsequent data packets are validated against the expected sequence number,
    copied into `client_to_network` (the TCP connection's `StreamBuffer`), and
    written to the host socket. ACKs are forged back to the device.
14. `Client::push_to_network` consumes the incoming packet; after the receive
    loop, `Client::process` recomputes the client's poll interests.

### 3.2 Internet → device (response path)

1. The host socket becomes readable. `TcpConnection::process_receive` checks
   that the device's advertised window allows more bytes and that no response
   packet is already pending.
2. It reads up to `min(remaining_client_window, MAX_PAYLOAD_LENGTH)` bytes into
   the `Packetizer`'s internal buffer. `MAX_PAYLOAD_LENGTH` is
   `0x4000 - 20 - 20 = 16344`.
3. `Packetizer` reuses the saved reference IPv4/TCP headers, swaps source and
   destination addresses and ports, sets the current sequence/acknowledgment
   numbers and `ACK|PSH`, updates lengths, and computes the IP and TCP
   checksums.
4. `TcpConnection::send_to_client` gives the packet to the `Client`, which
   appends its raw bytes to `network_to_client` (`StreamBuffer`, 16 × 64 KiB).
5. The client's interests become readable + writable. On writable,
   `Client::process_send` writes as much of the buffer as the socket accepts.
6. The Android client reads the byte stream, re-splits it using each IP header's
   total length, and writes each packet to the tun fd.
7. The kernel delivers the payload to the app's socket.

UDP follows the same shape with two differences: there is no handshake or
sequence state, and datagram boundaries are preserved by `DatagramBuffer`
instead of a byte-oriented `StreamBuffer`.

## 4. Module layers

The source is organized in layers. Reading outward-in makes the code easier to
navigate.

### Layer 0: entry point and control

| Module | Responsibility |
|--------|----------------|
| `main.rs` | Parse arguments, set logging threshold, dispatch, return exit code |
| `cli.rs` | Define commands/options, defaults, and validators |
| `executor.rs` | Implement commands with `adb` and start the relay |
| `adb_monitor.rs` | Talk to `adb`'s `host:track-devices` service |
| `logger.rs` | Threshold-based logging |

### Layer 1: relay lifecycle and dispatch

| Module | Responsibility |
|--------|----------------|
| `relay/relay.rs` | Create the selector and tunnel server; poll; periodic cleanup |
| `relay/selector.rs` | `mio::Poll` + slab of `Rc<dyn EventHandler>`; deferred removal |
| `relay/tunnel_server.rs` | Bind loopback, accept clients, assign ids, remove clients |
| `relay/client.rs` | One device stream; id handshake; packet framing; buffers |
| `relay/router.rs` | Map flow ids to connections; create/remove connections |
| `relay/connection.rs` | `Connection` trait, `ConnectionId`, logging macros |

### Layer 2: transport engines

| Module | Responsibility |
|--------|----------------|
| `relay/tcp_connection.rs` | TCP state machine, payload proxy, forged ACKs/FIN/RST |
| `relay/udp_connection.rs` | Connected UDP socket, datagram forwarding, idle timeout |
| `relay/packet_source.rs` | Pull interface for deferred TCP response packets |

### Layer 3: packet codec

| Module | Responsibility |
|--------|----------------|
| `relay/ipv4_header.rs` | IPv4 header parse/edit/checksum |
| `relay/transport_header.rs` | TCP/UDP enum dispatch |
| `relay/tcp_header.rs` | TCP header parse/edit/checksum |
| `relay/udp_header.rs` | UDP header parse/edit; checksum disabled |
| `relay/ipv4_packet.rs` | Combined packet view; split into headers + payload |
| `relay/ipv4_packet_buffer.rs` | Recover packet boundaries from a byte stream |
| `relay/packetizer.rs` | Build response packets from reference headers + payload |

### Layer 4: buffers and utilities

| Module | Responsibility |
|--------|----------------|
| `relay/byte_buffer.rs` | Linear byte buffer with peek/consume |
| `relay/stream_buffer.rs` | Circular byte buffer for TCP/stream data |
| `relay/datagram_buffer.rs` | Circular buffer that preserves datagram boundaries |
| `relay/datagram.rs` | Datagram traits and `ReadAdapter` |
| `relay/binary.rs` | Big-endian helpers, packet dump, pointer identity |
| `relay/net.rs` | Integer IP to `Ipv4Addr` conversion |

## 5. Process and thread model in detail

### 5.1 `main`

`main` does almost nothing:

```rust
let cli = Cli::parse();
if cli.verbose {
    logger::set_threshold(LevelFilter::Debug);
}
match cli.cmd.execute() { ... }
```

If the command returns an `ExecutionError`, `main` logs it and returns exit code
`3`. A successful command returns `ExitCode::SUCCESS`. The relay itself has no
special exit path; it runs until the process is terminated or an I/O error
escapes.

### 5.2 `run`: relay + client in parallel

`run` is the normal interactive command:

```text
run(serial, dns, routes, port)
  ├─ spawn thread: start(serial, dns, routes, port)
  ├─ install Ctrl+C handler: stop client, process::exit(0)
  └─ relay(port)            ← blocks in main thread
```

Why parallel? The relay must be listening by the time the Android client
finishes starting and connects. `start` involves `adb` calls and possibly an
installation sleep, so it runs in a separate thread. The ordering is best-effort,
not a correctness guarantee; the Android client is expected to retry its
connection if the relay is not ready yet.

### 5.3 `autorun`: monitor + relay

`autorun` spawns a thread for `autostart` and then runs the relay:

```text
autorun
  ├─ spawn thread: autostart(dns, routes, port)
  └─ relay(port)
```

`autostart` creates an `AdbMonitor`. The monitor connects to the local adb server
and subscribes to device-list changes. For each device that transitions to
`device`, the callback invokes `async_start`, which spawns a start thread. There
is no corresponding stop on disconnect: the monitor only starts clients.

Unlike `run`, `autorun` does **not** install a Ctrl+C handler. A normal Ctrl+C
terminates the process; the Android client is left running (until the user or
Android stops it).

### 5.4 `autostart`: monitor only

The `autostart` subcommand runs the same monitor loop but does not start the
relay. It is useful when the relay is managed separately (for example, a
long-running `gnirehtet relay` process). `AdbMonitor::monitor` loops forever:
when `track-devices` fails, it logs the error, restarts the adb daemon, waits,
and tries again.

### 5.5 The relay event loop

Once `relay::run(port)` is entered, all data-plane state lives on one thread:

```text
Selector::create()
TunnelServer::create(port, &mut selector)
loop {
    poll(timeout until next cleanup deadline)
    if deadline reached: clean up expired connections
    run handlers for all ready events
}
```

`Selector` owns the `mio::Poll` and a `Slab` of event handlers. Each registered
socket gets a `Token` (the slab index). The handler is an `Rc<dyn EventHandler>`
closure that captures the object's `Rc<RefCell<…>>` and calls `on_ready`.

There is no shared-memory synchronization in the data plane. `Rc` and `RefCell`
are safe here precisely because only the event-loop thread touches them. The
control-plane threads use owned strings and `adb` processes; they do not mutate
relay connection state.

## 6. Ownership and borrowing design

The relay uses `Rc<RefCell<T>>` for graph-like ownership. This is intentional:
the event loop needs to hand out references to sockets that own callbacks, and
connections need to reach back to their client.

### 6.1 Main ownership graph

```text
Selector (slab)
  ├─ handler for TunnelServer  ──► Rc<RefCell<TunnelServer>>
  ├─ handler for Client #n     ──► Rc<RefCell<Client>>
  ├─ handler for TCP stream    ──► Rc<RefCell<TcpConnection>>
  └─ handler for UDP socket    ──► Rc<RefCell<UdpConnection>>

TunnelServer
  └─ clients: Vec<Rc<RefCell<Client>>>

Client
  ├─ router: Router
  │    ├─ client: Weak<RefCell<Client>>       (back-reference, no cycle)
  │    └─ connections: Vec<Rc<RefCell<dyn Connection>>>
  ├─ close_listener: closure holding Weak<TunnelServer>
  └─ pending_packet_sources: Vec<Rc<RefCell<dyn PacketSource>>>

TcpConnection
  ├─ client: Weak<RefCell<Client>>
  └─ self_weak: Weak<RefCell<TcpConnection>>
```

Weak references break cycles. For example, a `TcpConnection` can ask its client
to remove it from the router, but it does not keep the client alive. The client's
close listener similarly does not keep the tunnel server alive.

### 6.2 Why `ClientChannel` exists

A connection's `send_to_network` is called from inside `Client` while the client
is already mutably borrowed. If the connection then tried to call
`client.borrow_mut()` to enqueue a response, it would panic at runtime.

`ClientChannel` is a narrow borrowed view containing only:

- `network_to_client: &mut StreamBuffer`
- `stream: &mut TcpStream`
- `token: Token`
- `interests: &mut Interest`

The client constructs it with `client.channel()` and passes it to the router and
connection. The connection can enqueue response packets and update poll interests
without re-borrowing the whole client. This is a key architectural trick; most
of the otherwise surprising `send_empty_packet_to_client` vs
`reply_empty_packet_to_client` split exists to respect it.

### 6.3 Deferred token removal

`mio` events for a poll batch are processed one by one. A handler may close and
remove another handler while the batch is still being iterated. `Selector` does
not drop slab entries immediately; it pushes their tokens into
`tokens_to_remove` and clears them **after** all events in the batch have run.
This avoids invalidating `Rc` clones or slab indices that are still referenced by
the current batch.

## 7. State machines

### 7.1 Tunnel client lifecycle

```text
accepted
   │
   ▼
send 4-byte id ──► relay packets ──► closed
   │                    │
   │                    ├─ EOF from device
   │                    ├─ read/write error
   │                    └─ error/hup event
   ▼
close: deregister, shutdown, close all connections, notify TunnelServer
```

The client is not authenticated and has no application-level heartbeat. A
connection exists until it is closed, its socket fails, or the process exits.

### 7.2 TCP connection state

The TCP proxy implements a deliberately small subset of RFC 793:

```text
Init ──SYN──► SynSent ──host connect──► SynReceived ──ACK──► Established
                                                              │
                    ┌─────────────────────────────────────────┤
                    │ device FIN                              │ host EOF
                    ▼                                         ▼
                 LastAck                                  FinWait1
                    │                                         │
                    │ ACK of FIN                              │ device FIN
                    ▼                                         ▼
                  close                                   Closing
                                                              │
                                                              │ ACK of FIN
                                                              ▼
                                                            close
```

`FinWait2` also exists when the device has already ACKed the relay's FIN and the
relay is waiting for the device's FIN. See [05](05-tcp-proxy.md) for the exact
transitions.

### 7.3 UDP lifetime

UDP has no handshake. A connection is created on the first datagram for a flow
and removed when `idle_since` is older than 120 seconds. The relay checks
expiration every 60 seconds, so an idle flow can remain for up to roughly 180
seconds after its last activity.

## 8. Design choices and their consequences

### 8.1 Loopback-only listener

`TunnelServer` binds `127.0.0.1`, not `0.0.0.0`. The only expected client is the
local adb server, which forwards the device's tunnel. This avoids exposing an
unauthenticated proxy to the network. If the port is already in use, startup
fails with an I/O error and `main` exits with code 3.

### 8.2 No length prefix on the tunnel

The wire protocol relies on IPv4 framing. This is efficient and matches the
Android client's existing packet stream, but it makes malformed input dangerous:
`Ipv4PacketBuffer` asserts that the version is 4, and header parsers trust that
the IP header and transport header are large enough. The relay assumes a trusted,
well-behaved local client rather than acting as a hostile-input parser.

### 8.3 Reference-header forging

The relay does not construct headers from scratch for every response. It saves
the first packet's headers and reverses addresses/ports. This is compact and
preserves fields such as ToS, TTL, IP ID, and the TCP window (TCP flags are
overwritten per packet), but it also means:

- TCP options are removed for responses (`shrink_options`), so the forged header
  is always 20 bytes.
- The TCP window copied into responses is whatever was in the reference header
  (normally the device's SYN window), not a separately maintained relay receive
  window.
- IP ID and TTL are reused rather than regenerated.

These are acceptable because the device is the only consumer of the forged
packets and the relay is not trying to be a full router.

### 8.4 Payload copying instead of splicing

The relay reads payload from one socket and writes it to another. It does not use
kernel zero-copy (`splice`, `sendfile`, or `MSG_ZEROCOPY`). This makes flow
control and header forging straightforward and keeps the implementation portable,
at the cost of an extra copy per direction.

### 8.5 No retransmission engine

The host kernel retransmits data on the real socket, and the Android TCP stack
retransmits on the device side. The relay only acknowledges what it has actually
accepted, and ignores out-of-order or duplicate device segments. It does not
maintain a retransmission queue. If the relay drops a device segment because its
internal buffer is full, it relies on the device to retransmit later. This is why
sequence-number validation and buffer-space checks are so important in
`TcpConnection`.

### 8.6 Bounded memory, variable behavior when full

Every client has bounded buffers:

- 64 KiB incoming packet buffer;
- 1 MiB outgoing stream buffer;
- per-TCP-flow 256 KiB incoming stream buffer;
- per-UDP-flow 256 KiB datagram buffer.

When a buffer is full, behavior depends on the protocol:

- TCP response path: the connection keeps the built packet pending and stops
  reading from the host socket until the client drains.
- TCP request path: a device segment is dropped if the flow buffer is full; the
  device TCP stack will retransmit.
- UDP: the datagram is dropped and logged.

## 9. What the reader should retain

- The relay is a **terminating TCP/UDP proxy**, not a packet forwarder.
- All data-plane state is owned by **one nonblocking event-loop thread**.
- The tunnel is a **raw IPv4 byte stream**; the IP header supplies framing.
- TCP needs a real state machine because the device believes it is talking to the
  remote host; UDP is mostly socket-per-flow plus a datagram queue.
- Response packets are **forged from a reference header**, not built from scratch.
- Backpressure is explicit and protocol-specific; buffers are bounded by design.
- The code assumes a trusted local client and deliberately does not implement
  defensive parsing for arbitrary hostile packets.

Continue with [02 — CLI, ADB, and control plane](02-cli-and-execution.md) for the
commands that create this topology, or jump to
[03 — Tunnel server and client](03-tunnel-and-client.md) for the data-plane
machinery.
