# Gnirehtet relay — documentation (`relay-rust/`)

> **Scope.** This directory documents the **host relay** in
> [`relay-rust/`](../../relay-rust) — the server side of _Gnirehtet_. It explains
> what the relay is responsible for, how a raw IPv4 packet becomes a real TCP or
> UDP conversation on the host, and why the implementation is structured the way
> it is.
>
> **Version.** Gnirehtet 2.5.1. The Rust source is the source of truth; this
> directory is a reading guide for the current tree. The former Java relay and
> the Gradle build are not part of this version.

## Contents

| Document | What it covers |
|----------|----------------|
| [01 — Architecture](01-architecture.md) | The big picture: where the relay fits, the process/thread model, module layers, ownership, state machines, and the two packet journeys |
| [02 — CLI, ADB, and control plane](02-cli-and-execution.md) | Command-line parsing, logging, `adb` invocation, APK discovery, install/version gating, `run`/`autorun`/`autostart`, Ctrl+C, and the ADB device monitor |
| [03 — Tunnel server, selector, and client](03-tunnel-and-client.md) | The `mio` event loop, `TunnelServer`, `Client`, client-id handshake, stream framing, buffers, interests, and the router |
| [04 — Packet model and forging](04-packet-model.md) | IPv4/TCP/UDP header views, parsing, checksums, `Packetizer`, MTU choices, and how response packets are forged |
| [05 — TCP proxy](05-tcp-proxy.md) | The TCP control block and state machine, handshake, sequence/acknowledgment accounting, flow control, FIN/RST handling, and deferred packets |
| [06 — UDP proxy and buffers](06-udp-and-buffering.md) | UDP sockets, `DatagramBuffer`, idle expiration, and the byte/stream/packet buffers used across the relay |
| [07 — Lifecycle, errors, and diagnostics](07-lifecycle-errors-and-diagnostics.md) | Cleanup, close propagation, error policy, panics versus warnings, logging, limits, and known edge cases |
| [08 — Testing, code map, and reference](08-testing-and-code-map.md) | Tests, build commands, file/type maps, constants, CLI reference, and an extension checklist |

The Android side is documented separately in [`docs/app/`](../app/README.md).
That directory describes the device app; this directory describes the program
that runs on the computer.

## 1. What the relay is

_Gnirehtet_ provides **reverse tethering**: an Android device uses the internet
connection of the computer it is connected to, without root. The Android app
creates a VPN interface and forwards raw IP packets; the relay on the computer
turns those packets into ordinary host TCP/UDP conversations.

The relay is therefore **not** a VPN server in the usual sense:

- it does not create a tun/tap interface on the host;
- it does not receive a pre-built tunnel packet from an app protocol;
- it does not implement a user-space network stack for the whole device;
- it only handles the packets the Android client sends through one `adb`
  connection.

It is closer to a **packet-aware TCP/UDP proxy**. For every flow it sees, it
opens a real host socket to the destination, copies payload in both directions,
and forges the IPv4/TCP/UDP headers needed for the device to believe it is
talking directly to the remote endpoint.

## 2. End-to-end path

```
       ANDROID DEVICE                                  COMPUTER (host)
┌──────────────────────────────┐        ┌──────────────────────────────────────┐
│ Android apps                 │        │                                      │
│   │ normal sockets            │        │   relay-rust                         │
│   ▼                           │        │   listens on 127.0.0.1:31416         │
│ Linux network stack           │        │        │                             │
│   route → VPN interface       │        │        │ real TCP/UDP sockets        │
│   (tun, 10.0.0.2/32)          │        │        ▼                             │
│   │ raw IPv4 packets          │        │   Internet                           │
│   ▼                           │        │                                      │
│ Gnirehtet Android client      │        │                                      │
│   │ LocalSocket "gnirehtet"   │        │                                      │
│   ▼                           │        │                                      │
│ adbd ── adb reverse ──────────┼────────┼──► tcp:31416                         │
└──────────────────────────────┘        └──────────────────────────────────────┘
```

The `adb reverse` mapping is created by the relay's control plane:

```text
adb reverse localabstract:gnirehtet tcp:31416
```

On the device, `adbd` listens on the abstract Unix socket `gnirehtet`. Every
byte written to that socket travels over the adb connection to
`127.0.0.1:31416` on the host, where the relay accepts it. The relay never sees
the device's USB interface directly; it only sees one TCP stream per connected
device.

## 3. What the relay is responsible for

1. **Accept the device tunnel.** Bind `127.0.0.1:<port>` (default `31416`) and
   accept client connections. Send the 4-byte client id immediately after a
   connection is accepted.
2. **Recover packet boundaries.** The tunnel is a byte stream, not a message
   stream. Read raw IPv4 bytes and split them using the IP header's total-length
   field.
3. **Classify each packet.** Parse IPv4 and then TCP or UDP. Build a flow key
   from protocol, source address/port, and destination address/port.
4. **Create or reuse a real host socket.** For TCP, open a host `TcpStream` and
   act as the peer of the device's TCP stack. For UDP, open a connected host
   `UdpSocket`.
5. **Proxy payload.** Copy device payload into the host socket and host responses
   back toward the device, preserving TCP sequencing and UDP datagram
   boundaries.
6. **Forge response packets.** Clone a reference header, swap addresses and
   ports, set lengths and checksums, and place the response payload into a raw
   IPv4 packet.
7. **Apply backpressure and cleanup.** Buffer data when a socket is not writable,
   defer TCP packets when the tunnel is full, drop UDP datagrams when necessary,
   and expire idle UDP flows.
8. **Provide the command-line control plane.** Install/start/stop the Android
   client, create the `adb reverse` tunnel, optionally monitor devices, and run
   the relay.

## 4. What the relay does not do

- **No Android UI or VPN setup.** Creating the VPN interface, obtaining consent,
  and reading/writing the tun fd belong to the Android client.
- **No DNS implementation.** DNS is just UDP/TCP traffic from the device. The
  client's DNS servers only configure the VPN; the relay forwards those packets
  like any other.
- **No IPv6.** The parser and flow key are IPv4-only. The Android client is
  expected to drop non-IPv4 packets before they reach the tunnel.
- **No TCP retransmission, congestion control, or reassembly.** The device TCP
  stack and the host kernel own those functions. The relay copies payload and
  maintains just enough TCP state to keep sequence/acknowledgment numbers
  coherent. See [05](05-tcp-proxy.md).
- **No encryption or authentication.** The listener is bound to loopback, and
  the trust boundary is the local machine plus the `adb` connection. The client
  id is a connectivity/debugging aid, not a credential.

## 5. Wire protocol in four points

1. **Transport.** One reliable, ordered byte stream per Android client, carried
   by `adb reverse` and the local TCP listener.
2. **Handshake.** The relay writes its client id as a **4-byte big-endian
   integer** before any packet data. This lets the Android client distinguish a
   real relay from a socket that was accepted while no relay was listening.
3. **Device → relay.** A stream of complete raw IPv4 packets. There is no length
   prefix; the relay reads the version/IHL and total length from the IPv4 header.
4. **Relay → device.** A stream of forged IPv4 packets. The relay does **not**
   add a length prefix either. The Android client re-splits the byte stream using
   the IP header. Network writes may coalesce multiple packets or split one
   packet across reads; both sides must tolerate that.

## 6. Component map

| Component | Role | Read more |
|-----------|------|-----------|
| `cli.rs` | `clap` command tree, defaults, validators, global `-v/--verbose` | [02](02-cli-and-execution.md) |
| `main.rs` | Parse CLI, set log threshold, dispatch command, map errors to exit code 3 | [02](02-cli-and-execution.md) |
| `logger.rs` | Lightweight global logger with an atomic `LevelFilter`; stdout/stderr output | [02](02-cli-and-execution.md) |
| `executor.rs` | Implement every CLI command; invoke `adb`; install/start/stop; run relay | [02](02-cli-and-execution.md) |
| `adb_monitor.rs` | `host:track-devices` client used by `autostart`/`autorun` | [02](02-cli-and-execution.md) |
| `relay/relay.rs` | Top-level relay loop, poll timeout, periodic cleanup | [03](03-tunnel-and-client.md) |
| `relay/selector.rs` | `mio::Poll` wrapper, slab of event handlers, deferred token removal | [03](03-tunnel-and-client.md) |
| `relay/tunnel_server.rs` | Loopback listener; accepts `Client`s; assigns ids; client removal | [03](03-tunnel-and-client.md) |
| `relay/client.rs` | One accepted device stream: id handshake, packet framing, buffers, `ClientChannel` | [03](03-tunnel-and-client.md) |
| `relay/router.rs` | Flow lookup; creates/removes TCP/UDP `Connection`s | [03](03-tunnel-and-client.md) |
| `relay/connection.rs` | `Connection` trait, `ConnectionId`, 10.0.2.2 → localhost rewrite, log macros | [03](03-tunnel-and-client.md) |
| `relay/tcp_connection.rs` | TCP proxy and state machine | [05](05-tcp-proxy.md) |
| `relay/udp_connection.rs` | UDP proxy and idle expiration | [06](06-udp-and-buffering.md) |
| `relay/packetizer.rs` | Build response packets from a reference header + payload | [04](04-packet-model.md) |
| `relay/ipv4_header.rs`, `tcp_header.rs`, `udp_header.rs`, `transport_header.rs` | Zero-copy header views and checksum logic | [04](04-packet-model.md) |
| `relay/ipv4_packet.rs` | Parsed packet: raw bytes, header data, payload access | [04](04-packet-model.md) |
| `relay/byte_buffer.rs`, `stream_buffer.rs`, `ipv4_packet_buffer.rs`, `datagram_buffer.rs` | Buffer primitives with different framing guarantees | [06](06-udp-and-buffering.md) |
| `relay/datagram.rs` | `DatagramSender`/`DatagramReceiver`, `ReadAdapter` | [06](06-udp-and-buffering.md) |
| `relay/binary.rs` | Big-endian helpers, packet dumps, thin-pointer comparison | [04](04-packet-model.md) |
| `relay/net.rs` | `u32` to `Ipv4Addr`/`SocketAddrV4` conversion | [04](04-packet-model.md) |

## 7. Process and thread model

The relay has a very small number of threads:

- **Main thread** — parses the command, performs control-plane work, and, for
  `relay`/`run`/`autorun`, runs the relay event loop. The event loop is the only
  thread that touches relay sockets, connections, buffers, or `Rc`/`RefCell`
  state.
- **Ctrl+C handling** — installed by the `ctrlc` crate for the `run` command.
  The callback sends a STOP intent to the client and exits the process.
- **Startup threads** — `run` and `autostart` call `async_start`, which spawns a
  short-lived thread per start operation so that `adb` installation/startup does
  not block the relay.
- **ADB monitor thread** — `autorun` spawns a thread that runs `AdbMonitor`,
  which talks to the adb server on `127.0.0.1:5037`. It invokes a callback for
  each newly connected device; the callback spawns another start thread.

The data plane is deliberately **single-threaded and nonblocking**. There is no
async runtime, no thread pool, and no locking on relay state. `mio` provides
readiness notifications, and each handler drains its socket until `WouldBlock`.
This keeps ordering simple and makes the TCP state machine deterministic, at the
cost of requiring careful buffer/interest management.

## 8. Reading order

1. [01 — Architecture](01-architecture.md) — build the mental model first.
2. [02 — CLI and control plane](02-cli-and-execution.md) — how the program gets
   from a command line to a running relay and a started Android client.
3. [03 — Tunnel server and client](03-tunnel-and-client.md) — how bytes enter
   and leave the relay, and how a flow is found or created.
4. [04 — Packet model](04-packet-model.md) — the byte-level representation and
   response-packet construction.
5. [05 — TCP proxy](05-tcp-proxy.md) — the most intricate part: TCP state and
   sequence accounting.
6. [06 — UDP and buffers](06-udp-and-buffering.md) — datagrams and the memory
   model behind backpressure.
7. [07 — Lifecycle and diagnostics](07-lifecycle-errors-and-diagnostics.md) —
   what happens when things close, fail, or misbehave.
8. [08 — Testing and reference](08-testing-and-code-map.md) — commands, maps, and
   extension notes.

## 9. Glossary

| Term | Meaning |
|------|---------|
| **Android client** | The app in [`app/`](../../app) that creates the VPN and forwards raw packets. |
| **tunnel** | The byte stream between the Android client and the relay, carried by `adb reverse`. |
| **tunnel client** | The relay's `Client` struct: one accepted device connection. |
| **flow / connection** | A TCP or UDP 4-tuple (protocol, source IP/port, destination IP/port). |
| **network socket** | The real host `TcpStream`/`UdpSocket` that talks to the internet. |
| **forged packet** | A raw IPv4 packet constructed by the relay as if it came from the remote endpoint. |
| **reference header** | The header of the first packet of a flow, cloned and reversed to build responses. |
| **`ClientChannel`** | A borrowed view of the client's outgoing buffer/socket used by a connection while the client is already mutably borrowed. |
| **pending packet source** | A TCP connection that has a built response packet but could not enqueue it because the client buffer was full. |
| **MTU** | Maximum transmission unit; the relay uses the same `0x4000` value as the Android client for TCP chunks. |

## 10. Source-of-truth note

The Rust code is authoritative. If this documentation and the code disagree,
read the code and update the documentation. The documentation intentionally
describes the current implementation rather than an idealized version: for
example, the relay does not implement TCP retransmission, and the APK lookup is
only the environment variable plus the current-directory filename.
