# 07 — Lifecycle, errors, and diagnostics

[← Previous: UDP and buffers](06-udp-and-buffering.md) · [Index](README.md) · [Next: testing and code map →](08-testing-and-code-map.md)

This chapter describes what happens when a client, connection, or the relay exits;
how errors are classified; when the code warns, closes, or panics; and how to use
the logs to diagnose a session. It also collects the known limitations that a
reader should keep in mind.

## 1. Relay process lifecycle

### 1.1 Startup

```text
main
  ├─ Cli::parse()
  ├─ if verbose: logger threshold = Debug
  └─ Command::execute()
       ├─ control-plane commands (install/start/stop/tunnel)
       └─ relay / run / autorun
            ├─ Selector::create()
            ├─ TunnelServer::create(port, selector)
            └─ poll_loop()
```

`TunnelServer::create` binds `127.0.0.1:<port>`. If binding fails, the error
propagates out of `Relay::run`, into `relay(port)`, into `Command::execute`, and
`main` logs `Execution error: ...` and returns exit code `3`.

### 1.2 Steady state

The relay remains in `poll_loop` indefinitely. There is no signal handler in the
relay module and no graceful-shutdown API. Stopping the relay means terminating
the process (Ctrl+C, SIGTERM, etc.). On process exit:

- the listener, client sockets, and connection sockets are closed by the OS;
- the Android client observes EOF on its tunnel and enters its reconnect path;
- the `adb reverse` mapping remains in place until removed or adb disconnects,
  but no host process is listening on the port.

There is no attempt to send a final RST/FIN to every flow during shutdown. The
host sockets are simply dropped by process teardown. This is acceptable for a local
proxy; TCP peers will see a connection reset/close and recover or fail according
to their own behavior.

### 1.3 Ctrl+C differences

| Command | Ctrl+C behavior |
|---------|-----------------|
| `run` | Handler sends STOP intent to the Android client, then `process::exit(0)`. |
| `autorun` | No handler; default termination. Android clients are not stopped. |
| `relay` | No handler; default termination. |
| `autostart` | No handler; default termination. Monitor stops, no clients stopped. |
| `start`/`stop`/`tunnel` | Short-lived; Ctrl+C terminates the adb command if still running. |

The `run` handler logs `Interrupted`, attempts the STOP intent, logs a failure if
it cannot stop, and exits `0` regardless. The relay itself never observes a
shutdown flag.

## 2. Cleanup loop

`Relay::poll_loop` maintains `next_cleaning_deadline`:

- initialized to `now + 120 seconds`;
- after each cleanup, set to `now + 60 seconds`.

When the deadline passes, it calls `TunnelServer::clean_up`:

```text
for each client:
    client.clean_expired_connections(selector)
        router.clean_expired_connections(selector)
            for each connection in reverse:
                if connection.is_expired():
                    connection.close(selector)
                    swap_remove(connection)
```

### 2.1 TCP never expires on idle

`TcpConnection::is_expired` always returns `false`. A TCP connection is removed
only by an explicit close path:

- device RST;
- host EOF/FIN with completed close handshake;
- read/write error;
- client tunnel close (`Router::clear`);
- a non-SYN first packet or conflicting duplicate SYN after sending RST.

If both endpoints disappear silently without a FIN or RST (for example, a network
black hole on the host side and a device that goes offline), the TCP connection
and its host socket remain until the process exits. There is no TCP idle timeout
or half-open scanner. This is a known limitation and a potential resource leak in
long-running relays.

### 2.2 UDP expiration timing

`UdpConnection::is_expired` is true when `idle_since.elapsed().as_secs() > 120`.
Because cleanup runs every 60 seconds, the observed lifetime after last activity
is between 120 and roughly 180 seconds. `touch()` runs at the start of every UDP
`process` call, including event notifications that do not transfer data.

### 2.3 Cleanup and borrow safety

`TunnelServer::clean_up` borrows each client mutably in turn and calls the router.
The router iterates connections in reverse because `swap_remove(i)` moves the last
element into position `i`. Forward iteration would skip the moved element or use
an index that is no longer valid.

## 3. Close propagation

### 3.1 Client close

`Client::close` is the root of a subtree shutdown:

```text
Client::close
  ├─ closed = true
  ├─ selector.deregister(stream, token)       // unwraps
  ├─ stream.shutdown(Both)
  ├─ router.clear(selector)
  │    └─ for each connection: close(selector)
  └─ close_listener.on_closed(self)
       └─ TunnelServer::remove_client(self)
            └─ clients.swap_remove(index)
```

The order matters:

1. Deregister the client so no new client events arrive.
2. Shut down the socket to unblock/close the transport.
3. Close all connections so their host sockets and tokens are released.
4. Notify the tunnel server so its strong `Rc<Client>` is removed.

`Client::close` uses `selector.deregister(...).unwrap()`. Unlike TCP/UDP close,
which logs deregistration failures as warnings, a client deregistration failure
panics. The assumption is that a live client is always registered; if that is
violated, it is an internal bug.

### 3.2 Connection close from a client

`Router::clear` calls `close` on every connection and then clears the vector. Each
connection's `close` deregisters or removes its token. The connection may still be
referenced by:

- a pending packet source (`Rc` held by the client), which is then dropped when the
  client is dropped;
- the current event batch's cloned handler `Rc`, which is dropped after the
  handler returns;
- the selector slab, whose token removal is deferred until the end of the batch.

No explicit `Rc` cycle keeps a closed connection alive.

### 3.3 Connection self-removal

When a connection closes while handling its own socket readiness event
(`TcpConnection::on_ready` / `UdpConnection::on_ready`), it calls
`remove_from_router`:

```text
self.client.upgrade()
  → borrow client
  → client.router().remove(self)
  → find by thin pointer
  → swap_remove
```

This is separate from the router-initiated path, where `Router::send_to_network`
checks `is_closed()` after calling the connection and removes it there.

Exactly one path should remove a given connection per close. The router checks
closed **after** `send_to_network`; the connection self-removes only from
`on_ready`. A double removal would panic with "Removing an unknown connection"
or "Trying to remove an unknown client", which is treated as an invariant
failure.

### 3.4 Tunnel server removal

The close listener is a boxed closure holding `Weak<TunnelServer>`. On client
close:

```text
weak.upgrade()
  → Some(server) → borrow_mut → remove_client(client)
  → None         → warn "no tunnel_server available"
```

`remove_client` compares the client pointer with entries in `clients` and uses
`swap_remove`. If not found, it panics. This is an internal consistency check.

## 4. Error classification

The code distinguishes several failure classes with different responses.

### 4.1 Control-plane errors (`ExecutionError`)

| Variant | Meaning | User-visible result |
|---------|---------|---------------------|
| `ProcessIo` | `adb` could not be spawned (not found, permission, etc.) | Error message; exit code 3 |
| `ProcessStatus` | `adb` ran but returned non-zero or was signaled | Error message with exit code/signal; exit code 3 |
| `Io` | Relay bind or other `io::Error` converted by `?` | `IO error: ...`; exit code 3 |
| `Ctrlc` | Ctrl+C handler installation failed | Error message; exit code 3 |

These are all reported through `main`'s `error!` and return `ExitCode::from(3)`.
`ProcessStatusError` on Unix distinguishes a normal exit code from signal
termination; on non-Unix, a status without a code panics with "Unexpected signal
termination on non-unix system".

### 4.2 Recoverable runtime events (warnings)

The relay logs and continues for conditions it can tolerate:

- client buffer full (`Client buffer full`);
- invalid/non-TCP/UDP packet (drop; trace dump if enabled);
- out-of-order or duplicate TCP segment (ignore; device retransmits);
- TCP write/read errors that lead to a RST and close (the connection is lost, but
  the relay continues);
- failure to deregister a TCP/UDP socket (warn; continue);
- failure to send an empty control packet (warn; there is no retry, so TCP
  recovery is not guaranteed; see [05 §7.4](05-tcp-proxy.md#74-client-buffer-full-deferred-packet));
- UDP datagram buffer full (drop);
- UDP response cannot be enqueued because the client buffer is full (drop);
- ADB monitor failure (log, restart adb daemon, retry).

Warnings are intentionally noisy enough to explain packet loss but do not stop the
relay. A failed TCP/UDP deregistration is only logged, not retried; in that rare
case the selector's slab entry can remain until the process exits. This is why
the connection close paths prefer a warning over a panic for that particular
`mio` operation.

### 4.3 Internal invariant failures (panics)

The relay uses `panic!`, `assert!`, `assert_eq!`, `expect`, and `unwrap` for
conditions that indicate a broken internal assumption or a protocol violation. A
panic in the data-plane thread terminates the relay process; there is no recovery
path. Examples:

- `Ipv4PacketBuffer` sees a non-IPv4 version:
  `assert!(version == 4, "Not an Ipv4 packet...")`.
- `Selector::run_handlers` cannot find the token: `Token not found`.
- A pending packet source has no packet:
  `Unexpected pending source with no packet`.
- A connection reports a non-WouldBlock error from `on_ready`:
  `Unexpected unhandled error`.
- `TcpConnection` tries to remove an unknown connection, or `TunnelServer` removes
  an unknown client.
- Parsing a packet with a truncated/malformed header: slicing or field reads may
  panic.
- `getrandom::u32()` fails (environment RNG failure).
- `Ipv4Packet::parse` receives a declared total length larger than the slice; the
  caller should have prevented this, so it is an invariant failure.

This policy reflects a **trusted local client**. The Android app is expected to
send valid IPv4 packets, and adb is local. The relay is not hardened against a
malicious process that can connect to `127.0.0.1:31416` and send arbitrary bytes;
such a process could trigger panics or resource exhaustion.

### 4.4 `WouldBlock` is not an error

The event loop is nonblocking. `WouldBlock` means "try again when ready" and is
handled explicitly:

- `Client::on_ready` logs a spurious event at `Debug` and ignores it.
- TCP/UDP `on_ready` log a spurious event at `Debug` and ignore it.
- Read/write loops break on `WouldBlock` and wait for the next readiness event.
- `DatagramBuffer::write_to` returns `WouldBlock` without consuming the datagram.

Never convert `WouldBlock` into a fatal error when modifying the loop; that would
turn normal backpressure into a disconnect.

## 5. Logging and diagnostics

### 5.1 Log format and destinations

```text
2026-10-22 14:05:12.481 INFO Relay: Relay server started
2026-10-22 14:05:13.002 DEBUG TcpConnection: 10.0.0.2:54321 -> 1.2.3.4:80 State = SynSent
```

- `ERROR` uses `stderr`; all other levels use `stdout`.
- Timestamp is local time with millisecond precision.
- The target is the module tag (`Relay`, `TunnelServer`, `Client`, `Router`,
  `TcpConnection`, `UdpConnection`, `AdbMonitor`, `Executor`, `Main`, etc.).
- Connection logs are prefixed with `ConnectionId` via the `cx_*!` macros.

If you pipe stdout and stderr separately, errors remain visible even if normal
output is filtered.

### 5.2 What to enable

- Default (`Info`): startup, client connect/disconnect, connection open/close,
  command progress. Good for normal operation and basic failures.
- `-v` (`Debug`): event-loop events, state transitions, interests, packet sizes,
  ACK/sequence details, tunnel cleanup decisions, command invocations. This is the
  normal debugging level.
- `Trace`: raw packet dumps (first 20 bytes), only reachable programmatically in
  this revision. If you are modifying the code, call
  `logger::set_threshold(LevelFilter::Trace)` early or add a temporary CLI level
  option. Do not expect `-v` to show hex dumps.

### 5.3 Useful log messages

| Message/target | Meaning |
|----------------|---------|
| `Relay server started` | Bind succeeded and the poll loop is entered. |
| `Client #N connected` / `disconnected` | Tunnel accept/close. |
| `Client id #N sent to client` | Handshake completed. |
| `Open` (connection) | A new TCP/UDP flow was created. |
| `Close` (connection) | A flow is being torn down. |
| `State = ...` (TCP) | State machine transition. |
| `interests: ...` (TCP/UDP) | Current mio interest; useful for stalls. |
| `Ignoring packet ... expecting ...` | TCP out-of-order/duplicate; usually benign. |
| `Not enough space, dropping packet` | TCP request buffer full; device should retransmit. |
| `Client buffer full` | TCP data response deferred, TCP control packet dropped, or UDP response dropped. |
| `Cannot send to client, drop packet` | UDP response lost under backpressure. |
| `Removing expired connection` | UDP idle timeout cleanup. |
| `Cannot accept client: ...` | Listener accept error (often transient). |
| `Failed to monitor adb devices` | ADB monitor/daemon issue. |

### 5.4 Debugging a stalled TCP connection

A useful sequence:

1. Run with `-v` and identify the flow's `ConnectionId` in the `Open` log.
2. Look for `State = ...` transitions. A connection stuck in `SynSent` means the
   host connect has not completed or was never detected.
3. Look for `interests: Some(READABLE)` / `Some(WRITABLE)` / `None`. If interests
   are `None` while the device is waiting, investigate `may_read`/`may_write`.
4. Look for repeated `Ignoring packet ... expecting ...`. A persistent mismatch
   means sequence accounting or a dropped segment is not recovering.
5. Look for `Not enough space` or `Client buffer full`. These indicate
   backpressure, not protocol corruption.
6. Check `Close` and the preceding error/RST message.

### 5.5 Debugging a UDP issue

1. Confirm the flow exists (`Open`/`Close` with `UdpConnection`).
2. Check `Datagram buffer is full` and `Cannot send to client, drop packet`.
3. Remember that UDP responses are dropped if the client tunnel buffer is full;
   there is no retry.
4. Check whether the flow is being expired after ~120–180 seconds of inactivity.

### 5.6 Debugging the control plane

- `Execute: "adb" [...]` at `Debug` shows the exact adb invocation.
- A `ProcessStatusError` includes the command and exit status.
- The version check is a `dumpsys package` call; if it fails, the relay does not
  install. Run the same command manually to see adb's output.
- `GNIREHTET_APK` and `ADB` environment variables are the two path overrides.
- `adb reverse --list` shows the current tunnel mapping.
- `adb devices -l` shows serials and states; the monitor only reacts to `device`.

## 6. Resource limits and exhaustion

The relay bounds memory per client/flow, but not the number of clients or flows.
Potential exhaustion paths:

- **Many clients.** `TunnelServer::clients` grows with each accepted connection.
  There is no connection limit or idle client timeout.
- **Many TCP flows.** Each connection allocates a 256 KiB `StreamBuffer` and a
  64 KiB `Packetizer` (plus socket state); the client itself also has a 64 KiB
  incoming buffer and a 1 MiB outgoing buffer shared by its flows. There is no
  global limit.
- **Many UDP flows.** Each allocates a datagram buffer (~320 KiB including the
  extra region) and a 64 KiB packetizer; idle flows expire after ~120–180 seconds,
  but active flows do not.
- **File descriptors.** Each client, TCP flow, and UDP flow holds at least one
  socket. The process is limited by the OS `RLIMIT_NOFILE`; errors from
  `accept`/`connect`/`bind` are logged and the affected operation fails.
- **Sequence number/next client id overflow.** `next_client_id` is a `u32`
  incremented for every accept; there is no special handling at overflow (which
  would require billions of accepts). TCP sequence numbers use `Wrapping`, so
  sequence wrap is handled there.
- **Panics from malformed input.** A local process that can connect to the
  loopback port can send non-IPv4 bytes and trigger the version assertion or
  truncated-header panics. The listener is not authenticated.

For a desktop single-device use case, these limits are not a concern. For a
long-running multi-device setup, add limits and hardening before exposing the port
beyond loopback.

## 7. Known limitations and quirks

### 7.1 Protocol scope

- IPv4 only; no IPv6.
- No IP fragmentation/reassembly. Non-initial fragments are misparsed as
  transport headers.
- TCP options are stripped from forged headers; no window scaling, SACK,
  timestamps, or MSS negotiation.
- UDP checksum is zero (disabled) for forged packets.
- Incoming checksums are not verified.

### 7.2 TCP behavior

- No retransmission timers, congestion control, or out-of-order queue.
- A segment whose sequence number is not exactly expected is ignored.
- A failed host connect is not detected via `take_error()`; the SYN-ACK may be
  sent and the failure observed on first I/O after the handshake. If the device
  never completes the handshake, the flow can remain half-open instead.
- The handshake-completing ACK returns before copying any piggybacked payload.
- `CloseWait` is declared but bypassed; `Established` + device FIN goes directly
  to `LastAck` after sending FIN.
- Empty control packets (SYN-ACK, ACK, FIN, RST) are not deferred when the client
  buffer is full; they can be lost and are not retried by the relay. A lost
  SYN-ACK can stall the flow because duplicate SYNs with the same ISN are ignored
  after `SynSent`; ACK-only loss has a similar risk.
- The deferred response-packet path advances the forged TCP sequence number by the
  whole IPv4 packet length rather than the TCP payload length, so a deferred
  packet can desynchronize the device's sequence space. See
  [05 §7.4](05-tcp-proxy.md#74-client-buffer-full-deferred-packet).
- No TCP idle timeout; silent half-open connections can persist for the process
  lifetime.
- The forged TCP window is copied from the reference header; the relay does not
  advertise its own receive window.

### 7.3 UDP behavior

- A full datagram buffer drops new datagrams.
- A full client buffer drops response datagrams.
- No retransmission or application-level reliability.
- Idle expiration is approximate (120–180 seconds depending on the cleanup
  phase).
- The relay does not preserve the host's source port; it only forges the remote
  endpoint's port in responses.

### 7.4 Control plane

- `run` and `autorun` have different Ctrl+C semantics; only `run` stops the
  Android client.
- `autorun`/`autostart` do not stop clients on device disconnect.
- APK lookup has no executable-directory fallback.
- Version gating checks only `versionCode`, not `versionName`.
- The 500 ms post-install sleep is a heuristic.
- ADB monitor retries use fixed 1 s/5 s delays, not exponential backoff.
- `autostart`/`autorun` can spawn overlapping start threads for a flapping device.

### 7.5 Security and trust

- The listener is loopback-only but unauthenticated.
- The client id is not a credential.
- The relay trusts packet contents from the local tunnel enough to use slicing and
  assertions; malformed input can crash the process.
- There is no encryption; the tunnel relies on adb's transport.

## 8. Failure-handling checklist

When a session fails, check in this order:

1. **Is the relay listening?** `ss -ltn 'sport = :31416'` or the startup log.
2. **Is the reverse tunnel present?** `adb reverse --list`.
3. **Is the Android client running?** Notification/state, or send a STOP/START
   cycle with `gnirehtet restart`.
4. **Did the relay accept a client?** Look for `Client #N connected` and
   `Client id #N sent`.
5. **Is the flow being created?** Look for `Open` with the expected
   `source -> destination`.
6. **Is TCP handshaking?** Look for `State = SynSent`, `SynReceived`,
   `Established`.
7. **Is data flowing?** Look for packet-size logs at `Debug` and for buffer-full
   warnings.
8. **Are there protocol resets?** Look for `Cannot write`, `Cannot read`, `RST`,
   `Close`.
9. **Is the device just slow to retry?** The Android client reconnects; a relay
   restart normally recovers after a delay.
10. **Do you need a packet dump?** `-v` gives Debug, not Trace; use host
    `tcpdump`/`tshark` on the loopback port or modify the threshold to Trace for a
    temporary build.

Continue with [08 — Testing, code map, and reference](08-testing-and-code-map.md).
