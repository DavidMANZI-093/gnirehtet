# 08 — Testing, code map, and reference

[← Previous: lifecycle and diagnostics](07-lifecycle-errors-and-diagnostics.md) · [Index](README.md)

This chapter is a lookup guide: how to build and test the relay, what the tests
actually cover, where each responsibility lives, and which constants define the
protocol and resource limits.

## 1. Build and test commands

The repository's `Makefile` is the canonical entry point. From the repository
root:

```bash
make relay          # cargo build (debug)
make relay-release  # cargo build --release (LTO enabled)
make relay-test     # cargo test
make relay-check    # cargo check
make relay-install  # cargo install --path relay-rust
make relay-clean    # cargo clean
```

Direct Cargo equivalents:

```bash
cargo build --manifest-path relay-rust/Cargo.toml
cargo build --release --manifest-path relay-rust/Cargo.toml
cargo test --manifest-path relay-rust/Cargo.toml
cargo check --manifest-path relay-rust/Cargo.toml
cargo install --path relay-rust
```

Quality gates used during development:

```bash
cargo fmt --manifest-path relay-rust/Cargo.toml -- --check
cargo clippy --manifest-path relay-rust/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path relay-rust/Cargo.toml
```

The release profile enables link-time optimization:

```toml
[profile.release]
lto = true
```

### 1.1 Dependencies and feature choices

The crate uses `edition = "2024"` and keeps dependencies intentionally small:

| Dependency | Purpose | Features |
|------------|---------|----------|
| `atomic_enum` | Atomic `LevelFilter` for the global logger | no default features |
| `chrono` | Local timestamps in logs | `clock`, no default features |
| `clap` | CLI parsing | `derive`, `std`, `help`, `usage`, `error-context`, `suggestions`, `wrap_help` |
| `ctrlc` | Ctrl+C handler for `run` | `termination` |
| `getrandom` | Random TCP initial sequence number | no default features |
| `mio` | Readiness-driven networking | `net`, `os-poll`, no default features |
| `slab` | Token/handler storage | default |

The binary does not use a regex crate for version parsing; `dumpsys` output is
parsed manually to keep the binary small.

## 2. Test inventory

`cargo test` currently runs 42 tests: 41 pass and 1 is ignored (the manual
checksum benchmark). The tests are unit tests colocated with their modules; there
are no integration tests that start a relay and drive real sockets.

| Module | Tests | Coverage |
|--------|-------|----------|
| `adb_monitor.rs` | 8 | Valid packet parsing, multiple packets, garbage/short packets, device/offline filtering, multiple devices, reconnect detection |
| `byte_buffer.rs` | 1 | Produce, consume, compact |
| `cli.rs` | 2 | Verbose default false; global `-v` before/after subcommand |
| `datagram.rs` | 4 | Mock send/recv, `ReadAdapter` unbounded and chunked |
| `datagram_buffer.rs` | 4 | Boundary preservation, zero-length datagram, circular wrap, `WouldBlock` retention |
| `ipv4_header.rs` | 5 | Parse, edit/swap, checksum, version/length peek |
| `ipv4_packet.rs` | 2 | Header parse and payload extraction |
| `ipv4_packet_buffer.rs` | 3 | Complete packet, fragmented read across calls, multiple packets |
| `packetizer.rs` | 3 | Header merge, `inflate`, chunked reads |
| `stream_buffer.rs` | 3 | Buffering, circular wrap, exact-fit fill |
| `tcp_header.rs` | 4 + 1 ignored | Edit/swap, checksums (even/odd/empty payload), manual checksum benchmark |
| `udp_header.rs` | 2 | Parse/edit/swap/length |

What is **not** covered by automated tests:

- `TcpConnection` state machine and sequence/acknowledgment behavior;
- `UdpConnection` event-loop behavior and idle expiration;
- `Client` handshake, buffering, and pending-source handling;
- `Router` creation/removal paths;
- `TunnelServer` accept/close paths;
- `Selector` registration/removal semantics;
- real `mio` readiness and edge-triggered draining;
- `executor` adb command composition (only the parser has tests);
- end-to-end packet transfer through a real device.

These areas are validated manually with a connected device. Be cautious when
changing them; the compiler and unit tests will not catch a TCP sequence bug or a
missed interest transition.

### 2.1 Manual checksum benchmark

`tcp_header::tests::bench_checksum` is `#[ignore]`. It computes 5 million TCP
checksums on a 1200-byte payload and prints the elapsed milliseconds. Run it with:

```bash
cargo test --manifest-path relay-rust/Cargo.toml \
  bench_checksum --release -- --nocapture --ignored
```

It is a microbenchmark, not a correctness test.

### 2.2 Smoke-testing a real device

A minimal end-to-end check (with the APK built/installed):

```bash
# Terminal 1: run the relay and start the client
GNIREHTET_APK=/path/to/gnirehtet.apk \
  ./relay-rust/target/release/gnirehtet -v run

# Terminal 2: verify the device path
adb shell ping -c 1 8.8.8.8
adb shell 'curl -sS https://example.com/ >/dev/null && echo ok'
```

If the device has multiple transports, pass the serial:

```bash
./relay-rust/target/release/gnirehtet -v run R58N73X3DXY
```

For a relay-only session:

```bash
# Terminal 1
./relay-rust/target/release/gnirehtet -v relay

# Terminal 2
./relay-rust/target/release/gnirehtet tunnel R58N73X3DXY
./relay-rust/target/release/gnirehtet start R58N73X3DXY
```

Stop it with `gnirehtet stop R58N73X3DXY` (the `relay` command does not handle
Ctrl+C by stopping the client).

## 3. File map

```text
relay-rust/
├── Cargo.toml
└── src/
    ├── main.rs                 entry point, logging threshold, exit code
    ├── lib.rs                  module exports
    ├── cli.rs                  clap definitions, defaults, validators, CLI tests
    ├── executor.rs             command implementations + ExecutionError
    ├── adb_monitor.rs          host:track-devices monitor
    ├── logger.rs               LevelFilter, emit, log macros
    └── relay/
        ├── mod.rs              relay module boundary and run()
        ├── relay.rs            Relay, poll loop, cleanup deadline
        ├── selector.rs         mio Poll + slab + deferred removal
        ├── tunnel_server.rs    listener, accept, client ids, removal
        ├── client.rs           tunnel client, id handshake, buffers, ClientChannel
        ├── router.rs           flow lookup, connection creation/removal
        ├── connection.rs       Connection trait, ConnectionId, cx_* macros
        ├── close_listener.rs   generic close callback trait
        ├── packet_source.rs    deferred TCP packet source
        ├── tcp_connection.rs   TCP proxy/state machine
        ├── udp_connection.rs   UDP proxy/idle timeout
        ├── packetizer.rs       forged packet builder
        ├── ipv4_header.rs      IPv4 header parse/edit/checksum
        ├── transport_header.rs TCP/UDP dispatch
        ├── tcp_header.rs       TCP header parse/edit/checksum
        ├── udp_header.rs       UDP header parse/edit
        ├── ipv4_packet.rs      parsed packet view/split/checksums
        ├── ipv4_packet_buffer.rs IPv4 framing over a byte stream
        ├── byte_buffer.rs      linear buffer
        ├── stream_buffer.rs    circular byte buffer
        ├── datagram_buffer.rs  circular datagram queue
        ├── datagram.rs         DatagramSender/Receiver, ReadAdapter, mocks
        ├── binary.rs           big-endian helpers, packet dump, pointer compare
        ├── net.rs              u32 IP conversions
        └── interrupt.rs        retry_on_intr! macro
```

## 4. Type reference

### 4.1 Control plane

| Type | Kind | Purpose |
|------|------|---------|
| `Cli` | struct | Parsed command line; `cmd`, `verbose` |
| `Command` | enum | All subcommands and their options |
| `ExecutionError` | enum | Control-plane error taxonomy |
| `ProcessIoError` | struct | Spawn failure with command context |
| `ProcessStatusError` | struct | Non-zero exit/signal with command context |
| `ProcessCommand` | struct | Command + args for display |
| `Termination` | enum | Exit value or Unix signal |
| `AdbMonitor` | struct | track-devices connection, buffer, connected list |

### 4.2 Relay data plane

| Type | Kind | Purpose |
|------|------|---------|
| `Relay` | struct | Port + poll loop |
| `Selector` | struct | `Poll`, handler slab, deferred removals |
| `EventHandler` | trait | `on_ready(selector, event)` |
| `TunnelServer` | struct | Listener, clients, next id |
| `Client` | struct | One tunnel connection and its router |
| `ClientChannel` | struct | Borrowed client egress view |
| `Router` | struct | Vec of connections + weak client |
| `Connection` | trait | `id`, `send_to_network`, `close`, `is_expired`, `is_closed` |
| `ConnectionId` | struct | Protocol + 4-tuple + log string |
| `CloseListener<T>` | trait | `on_closed(&T)` |
| `PacketSource` | trait | `get`, `next` for deferred TCP packets |

### 4.3 Transport engines

| Type | Kind | Purpose |
|------|------|---------|
| `TcpConnection` | struct | Host TcpStream + TCB + buffers |
| `Tcb` | struct | TCP state and sequence fields |
| `TcpState` | enum | Init/SynSent/SynReceived/Established/CloseWait/LastAck/Closing/FinWait1/FinWait2 |
| `UdpConnection` | struct | Host UdpSocket + datagram buffer + idle time |

### 4.4 Packet codec

| Type | Kind | Purpose |
|------|------|---------|
| `Ipv4HeaderData` | struct | Parsed IPv4 fields |
| `Ipv4Header<'a>` | struct | Read-only IPv4 view |
| `Ipv4HeaderMut<'a>` | struct | Mutable IPv4 view |
| `Protocol` | enum | Tcp/Udp/Other |
| `TransportHeaderData` | enum | Tcp/Udp data |
| `TransportHeader<'a>` | enum | Tcp/Udp read views |
| `TransportHeaderMut<'a>` | enum | Tcp/Udp mutable views |
| `TcpHeaderData` | struct | Parsed TCP fields |
| `TcpHeader<'a>` / `TcpHeaderMut<'a>` | struct | TCP views |
| `UdpHeaderData` | struct | Ports |
| `UdpHeader<'a>` / `UdpHeaderMut<'a>` | struct | UDP views |
| `Ipv4Packet<'a>` | struct | Full packet view |
| `Ipv4PacketBuffer` | struct | Framing buffer |
| `Packetizer` | struct | Forged packet builder |

### 4.5 Buffers and utilities

| Type | Kind | Purpose |
|------|------|---------|
| `ByteBuffer` | struct | Linear read/consume buffer |
| `StreamBuffer` | struct | Circular byte stream |
| `DatagramBuffer` | struct | Circular length-prefixed datagrams |
| `DatagramSender` / `DatagramReceiver` | traits | Socket-agnostic datagram I/O |
| `ReadAdapter` | struct | `Read` → `DatagramReceiver` with optional chunk limit |
| `LevelFilter` | enum | Off/Error/Warn/Info/Debug/Trace |

## 5. Constants

| Constant | Value | Meaning |
|----------|-------|---------|
| `DEFAULT_PORT` | `31416` | Relay and adb reverse port |
| `DEFAULT_DNS_SERVERS` | `8.8.8.8` | Client VPN DNS default |
| `DEFAULT_ROUTES` | `0.0.0.0/0` | Client VPN route default |
| `PACKAGE` | `d3fault.gnirehtet` | Android application id |
| `ACTIVITY` | `d3fault.gnirehtet/.GnirehtetActivity` | Control activity |
| `ACTION_START` | `d3fault.gnirehtet.START` | Start intent |
| `ACTION_STOP` | `d3fault.gnirehtet.STOP` | Stop intent |
| `REQUIRED_APK_VERSION_CODE` | `"9"` | Version gate |
| `TRACK_DEVICES_REQUEST` | `0012host:track-devices` | ADB host service request |
| `adb_monitor::BUFFER_SIZE` | `1024` | Track-devices parse buffer |
| `adb_monitor::RETRY_DELAY_ADB_DAEMON_OK` | 1 s | Retry after successful `adb start-server` |
| `adb_monitor::RETRY_DELAY_ADB_DAEMON_KO` | 5 s | Retry after failed `adb start-server` |
| `MAX_PACKET_LENGTH` | `1 << 16` = 65,536 | IPv4 packet/buffer bound |
| `MAX_DATAGRAM_LENGTH` | `1 << 16` = 65,536 | Datagram bound |
| `UDP_HEADER_LENGTH` | 8 | UDP header size |
| `MTU` | `0x4000` = 16,384 | TCP response MTU |
| `MAX_PAYLOAD_LENGTH` | `MTU - 20 - 20` = 16,344 | TCP read chunk cap |
| `IDLE_TIMEOUT_SECONDS` | `2 * 60` | UDP idle timeout |
| `CLEANING_INTERVAL` | 60 s | Relay cleanup period |
| `Events::with_capacity` | 1024 | Poll event batch size |
| `Slab::with_capacity` | 1024 | Initial handler capacity |
| `Client` outgoing buffer | `16 * 65536` | 1 MiB stream buffer |
| TCP incoming buffer | `4 * 65536` | 256 KiB stream buffer |
| UDP incoming buffer | `4 * 65536` | 256 KiB datagram capacity |

## 6. CLI reference

```text
gnirehtet [-v|--verbose] <COMMAND> [OPTIONS]
```

| Command | Options |
|---------|---------|
| `install [SERIAL]` | — |
| `uninstall [SERIAL]` | — |
| `reinstall [SERIAL]` | — |
| `run [SERIAL]` | `-d DNS[,DNS...]`, `-r ROUTE[,ROUTE...]`, `-p PORT` |
| `autorun` | `-d`, `-r`, `-p` |
| `start [SERIAL]` | `-d`, `-r`, `-p` |
| `autostart` | `-d`, `-r`, `-p` |
| `stop [SERIAL]` | — |
| `restart [SERIAL]` | `-d`, `-r`, `-p` |
| `tunnel [SERIAL]` | `-p PORT` |
| `relay` | `-p PORT` |

Defaults: DNS `8.8.8.8`, route `0.0.0.0/0`, port `31416`. `-v` is global and
sets the log threshold to Debug. There is no `-vv` and no CLI Trace level.

Environment variables:

| Variable | Effect |
|----------|--------|
| `ADB` | Path to the adb executable (default `adb`) |
| `GNIREHTET_APK` | APK path used by `install`/`reinstall`/`start`/`run` (default `gnirehtet.apk` in the working directory) |

## 7. Extension guide

### 7.1 Adding a CLI option

1. Add the field to the relevant `Command` variant in `cli.rs`.
2. Add an `#[arg(...)]` attribute with a value name, default, and validator.
3. Thread it through `Command::execute` and the corresponding executor function.
4. Add a parser test if the option has validation.
5. Update this reference table and [02](02-cli-and-execution.md).

Be careful with `global = true`: it propagates to subcommands and must be declared
once on `Cli` (as `verbose` is).

### 7.2 Adding a new transport protocol

The current router only recognizes TCP and UDP. To add another protocol:

1. Add a variant to `Protocol` in `ipv4_header.rs` and parse its number.
2. Add a `TransportHeaderData` variant and parser in `transport_header.rs`.
3. Add header view/edit/checksum code as needed.
4. Implement a `Connection` type and dispatch it in `Router::create_connection`.
5. Decide how `Packetizer` should build responses for it: copy/swap headers,
   update lengths, compute checksums.
6. Add framing/backpressure behavior. A datagram-like protocol can reuse
   `DatagramBuffer`; a stream-like protocol can reuse `StreamBuffer`.
7. Add tests and update the docs.

The parser's slices and the `Ipv4Packet` header assumptions are not defensive;
new protocols must respect the same trusted-input model or be hardened.

### 7.3 Changing buffer sizes

Buffer sizes are constants or constructor arguments:

- `TcpConnection::create`: `StreamBuffer::new(4 * MAX_PACKET_LENGTH)`.
- `UdpConnection::create`: `DatagramBuffer::new(4 * MAX_PACKET_LENGTH)`.
- `Client::create`: `StreamBuffer::new(16 * MAX_PACKET_LENGTH)`.
- `Ipv4PacketBuffer::new`: fixed `MAX_PACKET_LENGTH`.
- `Packetizer`: fixed `MAX_PACKET_LENGTH`.

Increasing buffers raises worst-case memory per client/flow. Decreasing them
increases drop/backpressure frequency. Keep the `Packetizer` buffer at least as
large as the largest packet you intend to forge; the TCP MTU constant assumes
20-byte IP and 20-byte TCP headers.

### 7.4 Changing the wire protocol

The wire protocol is intentionally minimal: 4-byte id, then raw IPv4 packets.
Changing it requires coordinated changes in the Android client
(`RelayTunnel`/`IPPacketOutputStream`). On the relay side:

- the id is written in `Client::send_id`;
- device→relay framing is `Ipv4PacketBuffer`;
- relay→device framing is just raw packet bytes in `Client::network_to_client`.

If you add a length prefix or a message header, update both directions and the
Android client at the same time; there is no version negotiation.

### 7.5 Hardening checklist

If the relay will ever accept untrusted connections, address these first:

- bind/authenticate beyond loopback;
- validate IPv4 version, IHL, total length, and transport header lengths before
  slicing;
- reject or reassemble IP fragments;
- validate incoming checksums;
- bound the number of clients and flows;
- add TCP idle/half-open timeouts;
- handle `take_error()` on nonblocking connect;
- avoid panics on malformed input and on deregistration failures;
- add fuzz/integration tests for the packet parser and TCP state machine.

## 8. Documentation map

| If you want to understand… | Read |
|----------------------------|------|
| Why the relay exists and how the pieces fit | [README](README.md), [01](01-architecture.md) |
| How a command starts a session | [02](02-cli-and-execution.md) |
| How the tunnel and event loop work | [03](03-tunnel-and-client.md) |
| How headers are parsed and response packets are built | [04](04-packet-model.md) |
| How TCP is terminated and proxied | [05](05-tcp-proxy.md) |
| How UDP and buffers work | [06](06-udp-and-buffering.md) |
| What happens on close/failure and how to debug | [07](07-lifecycle-errors-and-diagnostics.md) |
| Where a type or constant lives | This chapter |
| The Android client side | [`docs/app/`](../app/README.md) |

## 9. Documentation provenance

This documentation describes the current Rust implementation at version `2.5.1`.
It was written from the source in `relay-rust/src` and validated against the
current test suite (41 passing tests, 1 ignored benchmark). The code remains the
source of truth: if a future change alters behavior, update the corresponding
chapter rather than assuming the prose is still correct.
