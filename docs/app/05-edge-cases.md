# 05 — Edge cases, workarounds and quirks

[← Previous: packet framing](04-packet-framing.md) · [Index](README.md) · [Next: class reference →](06-class-reference.md)

Almost every "weird" line in this code base is a workaround for a concrete
platform behavior or race. This chapter collects them: what the problem was,
what the code does, and what remains imperfect. It is the best companion when
debugging strange behavior.

## 1. Waking up a blocking read on the VPN interface

**Problem.** When the user stops gnirehtet, the `Forwarder`'s device→tunnel
thread is normally parked in `vpnInput.read(buffer)`. Closing the VPN file
descriptor (`vpnInterface.close()`) does **not** wake that read — a well-known
Linux behavior that Android inherits. Closing the fd also does not make the
blocked thread return; it may stay blocked indefinitely on a descriptor that no
longer exists. Interrupting the thread does not help either
(`FileInputStream` is not interruptible). `Forwarder` would therefore keep a
stuck thread per stop.

**Workaround.** Give the kernel a reason to deliver a packet to the VPN
interface, so that the pending `read()` returns:

```java
private void wakeUpReadWorkaround() {
    EXECUTOR_SERVICE.execute(() -> {
        try {
            DatagramSocket socket = new DatagramSocket();
            InetAddress dummyAddr = InetAddress.getByAddress(DUMMY_ADDRESS); // 42.42.42.42
            DatagramPacket packet = new DatagramPacket(new byte[0], 0, dummyAddr, DUMMY_PORT /* 4242 */);
            socket.send(packet);
        } catch (IOException e) {
            // ignore
        }
    });
}
```

Mechanics, step by step:

1. The empty UDP datagram is emitted by the system on behalf of the app; with
   the default `0.0.0.0/0` route it is routed into the VPN interface.
2. The blocked `read()` returns the datagram's IPv4 packet (28 bytes: IP + UDP
   header, no payload).
3. `Forwarder` sees a valid IPv4 packet and calls `tunnel.send(...)`, which is
   `PersistentRelayTunnel.send(...)`. Its `stopped` flag is already `true`
   (set by `Forwarder.stop()` → `tunnel.close()`), so it throws
   `InterruptedIOException("Persistent tunnel stopped")` **without touching the
   network**.
4. The task catches `InterruptedIOException`, logs "Device to tunnel
   interrupted", and exits.

History (commits `0d832e0`, `cc367ff`): the first version triggered a fake DNS
resolution (`InetAddress.getByName("__fake_request__")`) to generate traffic;
but the resolver retries unanswered queries, which can leak several packets
onto the real network before the tunnel is closed. A single meaningless UDP
packet is deterministic by comparison.

Constraints and caveats:

- The datagram must be routed into the VPN. With the default route it is.
  With **custom routes** (`-r` / `routes` extra) that do not cover
  `42.42.42.42`, the packet leaves through another interface — the VPN read is
  not woken, one forwarding thread can stay blocked, and if the app is used
  again the shared 3-thread pool may lose a slot. In that case, also include
  `42.42.42.42/32` (or `0.0.0.0/0`) in the routes, or expect a lingered thread.
- The task runs on the executor because Android forbids network operations on
  the main thread (`NetworkOnMainThreadException`).
- `socket.send()` is best-effort: any `IOException` (e.g. no route) is
  swallowed. There is no retry.
- The packet is not "lost" in a meaningful way: it is a deliberately invalid
  destination/port combination (`.42.42.42.42:4242`), and the tunnel is closed
  anyway.

## 2. Detecting a dead relay behind `adb reverse`

**Problem.** `adb reverse` makes `adbd` accept TCP connections *locally* on the
device even if nothing is listening on the host side. So `connect()` on the
tunnel succeeds vacuously; the failure only surfaces on the first read. The
same false positive appears when the relay runs behind an SSH port forward.

**Workaround.** A one-read handshake: the relay immediately writes the client
id (4-byte big-endian) when a client connects; the client reads it in
`RelayTunnel.connect()` before declaring success. If there is no relay behind
the tunnel, this read fails (or blocks until the double-ended connection
breaks) instead of yielding a "connected" state followed by an immediate
"disconnected" flip.

History: commit `301867e` first mitigated this with a zero-ish read timeout on
a TCP socket, which did not cover the SSH-forward case; commit `47fae70`
replaced it with the id handshake in the protocol (both relay flavors were
updated).

Corollary: the relay **must** send the id before any packet, in both protocol
implementations; it is a protocol invariant, not an implementation detail.

## 3. IPv6 and other non-IPv4 traffic

- **Device → client.** Although the VPN is only configured with an IPv4
  route/address, some devices still forward IPv6 packets to the VpnService
  (issue #69). `Forwarder` checks the version nibble of the first byte and
  drops non-IPv4 packets with `Log.w(TAG, "Unexpected packet IP version: …")`.
  The relay server only implements IPv4 over TCP/UDP anyway.
- **Relay → client.** The protocol should only ever carry IPv4. As a defensive
  measure, `IPPacketOutputStream.sinkPacket()` logs and **clears its whole
  buffer** if the head of the stream is not IPv4 (see [04](04-packet-framing.md)).
- **Consequence.** IPv6-only connectivity does not work (native IPv6 internet,
  IPv6-only APNs, etc.). ICMP is not relayed either: the relay handles only
  TCP and UDP payloads.

## 4. Data-plane races and corner cases

### 4.1 Stale tunnel after a failed connect (one extra attempt)

When `RelayTunnel.connect()` throws, `RelayTunnelProvider.connectTunnel()`
notifies failure but leaves the freshly created (unconnected) `RelayTunnel` in
its `tunnel` field. The caller's local variable is still `null`, so it does not
invalidate it. On the next loop iteration, `getCurrentTunnel()` returns that
broken instance immediately (no backoff check), the immediate I/O fails, and
*now* the caller has a non-null reference and invalidates it properly.

Net effect: after every failed connection attempt there is one extra,
immediate failed I/O attempt. It wastes a little CPU and produces an extra
error log line, but the 5-second backoff is then correctly enforced on the next
attempt (the invalidation refreshed `lastFailureTimestamp`). No user-visible
impact beyond potentially duplicated "disconnected" events, which the
`Notifier` coalesces because it only acts on state *changes*.

### 4.2 Shutdown window: `getCurrentTunnel()` can return `null`

`getCurrentTunnel()` ends with `return tunnel;` *outside* both monitors. In the
tiny window between releasing the locks and reading the field, a concurrent
unconditional `invalidateTunnel()` (only invoked by `PersistentRelayTunnel.close()`
during shutdown) may set the field to `null`. The caller then dereferences
`null` and throws an `NullPointerException` — from a task whose `Future` has
just been cancelled, and whose exception is therefore swallowed. Practical
impact: none besides a possibly different shutdown log line. It is documented
here because a reader of the code will notice it.

### 4.3 Why invalidating is identity-checked

Thread A may still hold a reference to tunnel *T1* and hit an I/O error after
thread B has already reconnected and installed *T2*. Without the check
`tunnel == tunnelToInvalidate`, A would kill a perfectly healthy T2 and force
another 5 s outage. Hence the two variants:

- `invalidateTunnel()` — unconditional, used when the failure is known to
  concern the current tunnel (or during shutdown, where everything must die).
- `invalidateTunnel(Tunnel t)` — no-op unless `t` is still the current tunnel,
  used by the retry loops after a failed operation.

### 4.4 Backoff details

- `DELAY_BETWEEN_ATTEMPTS_MS = 5000`, counted from `lastFailureTimestamp`.
- `touchFailure()` happens on failed connect *and* on invalidation, so a
  connection that lives 1 ms or 1 hour still gets a full 5 s pause after it
  dies.
- The first attempt is never delayed (`if (first) return;`), so startup is
  fast.
- There is no `notifyAll()`; waiting threads rely on `wait(timeout)` waking up.
  A stop during a backoff is handled by interrupting the thread
  (`Future.cancel(true)`), which throws `InterruptedException`, which is caught
  by the retry loop, which then notices `stopped` and exits.

### 4.5 Packet retention and ordering during an outage

`PersistentRelayTunnel.send()` retains the packet argument across retries.
Consequences:

- **No silent loss:** the first packet of the outage is sent after
  reconnection, and the device→tunnel thread cannot read further packets until
  then — the tun queue fills and the device's TCP stacks back off.
- **Order is preserved.**
- **Latency spike:** after a long outage, the first packet is stale but
  harmless; retransmissions at the TCP level sort things out.

### 4.6 `RelayTunnel.close()` failure path

`close()` wraps any `IOException` in a `RuntimeException`. This is the only
unchecked failure path in the data plane: it would propagate through
`invalidateTunnel()` → `Forwarder.stop()` → `GnirehtetService.close()`, which
catches only `IOException` — a crash on the main thread in that pathological
case (e.g., socket already fully closed in a race). It has not been observed in
practice; it is the kind of thing to know about when debugging a rare crash
trace containing `RelayTunnel.close`.

## 5. Notification and service-lifecycle quirks

### 5.1 Optimistic "enabled" state

The notification says "Reverse tethering enabled" from the very moment
`notifier.start()` runs — before any relay connection is attempted. If the
first attempt fails, `RelayTunnelProvider`'s `first` flag guarantees a
"disconnected" event (see [03](03-data-plane.md)), so the optimistic state is
short-lived. But if the relay is unreachable *and* the first connection
attempt is still in progress (inside a slow `connect()`), users may see
"enabled" for a while.

### 5.2 Obsolete events after STOP

Tunnel events are posted to the main thread and may be delivered after a STOP
has already destroyed the session (between `close()` and message processing).
The handler drops them if `!isRunning()`. Without this check, a late
"disconnected" message would call `setFailure(true)` on a `Notifier` whose
`stop()` was already called — which would recreate a notification for a dead
session (an old bug class, fixed by the `isRunning()` guard).

### 5.3 `stopForeground(true)` and the channel

`Notifier.stop()` removes the notification (`stopForeground(true)`) and deletes
the `"Gnirehtet"` channel on O+. A subsequent start recreates the channel with
`IMPORTANCE_DEFAULT`. Because the notification is the only UI, the *Stop*
action is the only in-app way to stop.

### 5.4 `establish()` failure leaves the notification behind

`startVpn()` calls `notifier.start()` *before* `setupVpn()`. If
`establish()` returns `null` (consent revoked, preparation lost), no
`Forwarder` is created, but the foreground notification stays — and a following
STOP intent does nothing, because `close()` returns immediately when
`!isRunning()`. The notification then lingers until the process dies or a
later successful start/stop cycle removes it. This path is rare (the system
usually kills the app when the VPN is revoked).

### 5.5 The service is never stopped explicitly

After a successful `close()`, there is no `stopSelf()` call anywhere. The
service remains a *started* (but no longer foreground) service. On modern
Android, background started services are subject to the background execution
limits and are eventually killed by the system; so in practice the process
disappears after the user stops gnirehtet, but not instantly. Consequences:

- A new START before the system reaps the service reuses the same instance and
  thread pool.
- The system may also kill the process at any time for the same reason.

### 5.6 STOP when nothing is running (Android O+)

`GnirehtetService.stop()` uses `startForegroundService()` on API ≥ 26. When the
service is already running, it is foreground and everything is fine. But if the
process/service is dead and someone sends STOP, the service is started with the
`CLOSE_VPN` action, `close()` finds `isRunning() == false` and returns without
ever calling `startForeground()`. Android O+ expects a service started via
`startForegroundService()` to post its foreground notification within about
five seconds and otherwise raises a remote-service exception, so this *specific*
path can produce a logged crash of the client process (the practical effect is
nil — there was nothing to stop). The desktop CLI only sends STOP while
managing a running session, which is why this does not bite in normal use.

### 5.7 `START_NOT_STICKY` and revocations

- `onStartCommand` returns `START_NOT_STICKY`: if Android kills the process
  (low memory, revocation), the service is **not** restarted; the user must run
  the client again. There is no "always-on VPN" system integration.
- `onRevoke()` and `onDestroy()` are not overridden; there is no orderly
  cleanup if the system tears the service down. Threads die with the process,
  which is normally what happens anyway.
- The VPN permission can be revoked in Settings while running; Android stops
  the app, `vpnInterface` becomes invalid, and the data plane dies with the
  process.

## 6. Custom routes: operational caveats

The `routes` extra (CLI `-r`) makes the VPN capture only the listed CIDRs, so
that other traffic uses the device's own connectivity (e.g. Wi-Fi). Things to
know:

- `42.42.42.42:4242` wake-up traffic must be captured for a clean stop (see
  §1). Add `42.42.42.42/32` if you customize routes without a default route.
- DNS: with no `dnsServers` extra, the interface advertises `8.8.8.8`. Name
  resolution sends UDP queries to that address; if the custom routes do not
  cover it, the queries go out over the device's own networks instead (if any)
  and never reach the relay. For a device with no other connectivity you must
  pass `-d <server>` with a server address that falls inside a captured route
  and is reachable from the computer.
- Any address outside the captured routes simply does not use reverse
  tethering; that is the intended feature, not a bug.

## 7. Logging and diagnostics

| Tag | Emitted by | Typical lines |
|-----|-----------|---------------|
| `GnirehtetActivity` | activity | `Received request com.genymobile.gnirehtet.START`, `VPN was already authorized`, `VPN requires the authorization from the user, requesting...` |
| `GnirehtetService` | service | `Received request …`, `VPN already running, ignore START request`, `Relay tunnel connected/disconnected`, `VPN starting failed, please retry` |
| `Forwarder` | forwarding threads | `Device to tunnel forwarding started/stopped`, `Tunnel to device forwarding started/stopped`, `… interrupted`, `… exception`, `Unexpected packet IP version: N` |
| `RelayTunnel` | tunnel | `Opening a new relay tunnel...`, `Requesting client id`, `Connected to the relay server as #N` |
| `PersistentRelayTunnel` | retry loops | `Cannot send to tunnel` / `Cannot receive from tunnel` (+ stack traces), `Tunnel read EOF` |
| `RelayTunnelProvider` | provider | (no logs; it only notifies) |
| `CIDR` | parsing | `Error: …` for invalid CIDRs |

Typical commands:

```bash
adb logcat -s GnirehtetActivity GnirehtetService Forwarder RelayTunnel PersistentRelayTunnel
# packet-level (set GnirehtetService.VERBOSE = true and rebuild first):
adb logcat -s RelayTunnel
```

Reading a normal stop in the logs should show:

```
Forwarder   : Device to tunnel forwarding stopped
PersistentRelayTunnel: Cannot receive from tunnel (IOException…)
RelayTunnel : Tunnel read EOF
GnirehtetService: Relay tunnel disconnected
Forwarder   : Device to tunnel interrupted
```

(Exact interleaving varies; "interrupted" lines are normal, not errors.)

## 8. Naming and vestigial-code notes

| Item | Note |
|------|------|
| `GnirehtetService.setAsUndernlyingNetwork()` | Typo for "Underlying"; keep the spelled name when grepping or referencing. |
| `TestIPPacketOutputSteam` | Typo for "Stream" in the test class name. |
| `readPacketLength` Javadoc | Contains the typo "assuming thatan IP packet". |
| `Net.getLocalhostIPv4()` | Unused leftover from the era when the tunnel was a loopback TCP connection. |
| `RelayTunnel.open(VpnService)` | Parameter unused since the switch to `LocalSocket`; kept for API stability of internal callers. |
| `DEVELOP.md` ("Client" section) | References an `AuthorizationActivity` that was removed in commit `f868180`; the activity is now `GnirehtetActivity`. Also describes the tunnel as "a TCP connection" loosely. This documentation supersedes it for the app. |
| `VpnConfiguration` extras | `GnirehtetService` treats a missing extra as "use defaults"; the activity treats missing arrays as empty arrays. Two layers of the same fallback. |

## 9. Known limitations (summary)

1. **IPv4 only.** IPv6 is discarded; ICMP is not relayed.
2. **The client is not autonomous.** No relay and no `adb reverse` means no
   internet: the VPN still captures traffic, but packets are held in retry
   loops and typically time out.
3. **No encryption.** The protocol assumes the adb channel is trusted.
4. **No configuration persistence.** Every start must carry its options; there
   is no UI.
5. **One tunnel per device.** Multiple devices are handled by the relay side
   (each client gets its own connection/id); the app itself is per-device.
6. **StrictMode/threading assumptions.** A single process, a fixed 3-thread
   executor, no wake locks (the foreground service and notification keep the
   process alive, but there is no guarantee against Doze/App Standby
   restrictions on modern Android for prolonged screen-off use).

[← Previous: packet framing](04-packet-framing.md) · [Index](README.md) · [Next: class reference →](06-class-reference.md)
