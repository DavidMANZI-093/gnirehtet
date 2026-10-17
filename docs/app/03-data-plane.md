# 03 — Data plane: forwarding and the persistent tunnel

[← Previous: control plane](02-control-plane.md) · [Index](README.md) · [Next: packet framing →](04-packet-framing.md)

This chapter covers the classes that move bytes: `Forwarder`, `Tunnel`,
`RelayTunnel`, `RelayTunnelProvider`, `PersistentRelayTunnel` and
`RelayTunnelListener`. This is where the concurrency of the client lives, so
the synchronization schemes get special attention.

## 1. The pipeline at a glance

```
                  Forwarder (owns a fixed 3-thread executor)
   ┌──────────────────────────────────────────────────────────────┐
   │                                                              │
   │  thread 1:  VPN fd ──read()──► Tunnel.send() ──┐             │
   │                                                ▼             │
   │                                    PersistentRelayTunnel     │
   │                                     │ (retry loops)          │
   │                                     ▼                        │
   │                                RelayTunnelProvider           │
   │                                 (cache + reconnect)          │
   │                                     │                        │
   │                                     ▼                        │
   │                                RelayTunnel  ◄── LocalSocket │
   │                                     │                        │
   │  thread 2:  VPN fd ◄─write()── IPPacketOutputStream ◄─receive()
   │                                                              │
   │  thread 3 (transient): wake-up datagram in stop()            │
   └──────────────────────────────────────────────────────────────┘
                                     │
                     connected/disconnected events
                                     ▼
                          RelayTunnelListener → Handler (main)
                                     ▼
                                  Notifier
```

The interfaces are small on purpose:

```java
public interface Tunnel {
    void send(byte[] packet, int len) throws IOException;  // blocking
    int receive(byte[] packet) throws IOException;         // blocking
    void close();                                          // blocking
}
```

`Tunnel` is deliberately blocking: the two directions are just two threads.
There is no selector, no callback, no queue — which is what makes the whole
data plane easy to reason about.

## 2. `Forwarder`

[Source](../../app/src/main/java/com/genymobile/gnirehtet/Forwarder.java)

### 2.1 Constants and fields

| Member | Value | Purpose |
|--------|-------|---------|
| `EXECUTOR_SERVICE` | `Executors.newFixedThreadPool(3)` (static) | background threads for both directions + wake-up task |
| `BUFSIZE` | `0x10000` (65536) | read buffer for both directions — large enough for any IPv4 packet |
| `DUMMY_ADDRESS` / `DUMMY_PORT` | `42.42.42.42` / `4242` | destination of the wake-up datagram, see [05](05-edge-cases.md) |
| `vpnFileDescriptor` | `FileDescriptor` | the tun device, from `vpnInterface.getFileDescriptor()` |
| `tunnel` | `PersistentRelayTunnel` | the packet transport |
| `deviceToTunnelFuture`, `tunnelToDeviceFuture` | `Future<?>` | handles used by `stop()` |

The constructor takes the `VpnService` (used to build the tunnel), the fd, and
a `RelayTunnelListener`; it creates the `PersistentRelayTunnel` immediately.

### 2.2 `forward()`

```java
public void forward() {
    deviceToTunnelFuture = EXECUTOR_SERVICE.submit(() -> { … forwardDeviceToTunnel(tunnel); … });
    tunnelToDeviceFuture = EXECUTOR_SERVICE.submit(() -> { … forwardTunnelToDevice(tunnel); … });
}
```

Both tasks catch `InterruptedIOException` (logged as an expected
interruption — this is how a normal shutdown is reported) and any other
`IOException` as an error. Both call into the **same** `PersistentRelayTunnel`
instance; all the sharing/synchronization happens below this class, in the
provider.

### 2.3 Device → tunnel

```java
private void forwardDeviceToTunnel(Tunnel tunnel) throws IOException {
    FileInputStream vpnInput = new FileInputStream(vpnFileDescriptor);
    byte[] buffer = new byte[BUFSIZE];
    while (true) {
        int r = vpnInput.read(buffer);      // blocking read
        if (r == -1) { Log.d(TAG, "VPN closed"); break; }
        if (r > 0) {
            int version = buffer[0] >> 4;
            if (version == 4) {
                tunnel.send(buffer, r);     // blocking send
            } else {
                // see <https://github.com/Genymobile/gnirehtet/issues/69>
                Log.w(TAG, "Unexpected packet IP version: " + version);
            }
        } else {
            Log.d(TAG, "Empty read");
        }
    }
}
```

Notes:

- The read is blocking because `setupVpn()` called `builder.setBlocking(true)`
  (and because `FileInputStream` is used directly rather than a selector).
- A tun device returns **one packet per read**, so `r` is the length of one
  packet. The version check on the first nibble is a safety net: some devices
  push IPv6 packets into the VPN interface even though IPv6 was never enabled
  (issue #69). Such packets are dropped here — the relay would not know what
  to do with them.
- `tunnel.send()` is *blocking and retrying*: during a relay outage it may hold
  the packet for seconds (up to forever), preserving ordering and providing
  natural backpressure (the tun queue fills, TCP senders stall). See
  `PersistentRelayTunnel` below.
- `r == -1` (EOF) means the VPN interface is gone (normally only during
  shutdown); the loop exits and the task ends.

### 2.4 Tunnel → device

```java
private void forwardTunnelToDevice(Tunnel tunnel) throws IOException {
    FileOutputStream vpnOutput = new FileOutputStream(vpnFileDescriptor);
    IPPacketOutputStream packetOutputStream = new IPPacketOutputStream(vpnOutput);

    byte[] buffer = new byte[BUFSIZE];
    while (true) {
        int w = tunnel.receive(buffer);     // blocking receive
        if (w == -1) { Log.d(TAG, "Tunnel closed"); break; }
        if (w > 0) {
            packetOutputStream.write(buffer, 0, w); // blocking, re-frames packets
        } else {
            Log.d(TAG, "Empty write");
        }
    }
}
```

- `tunnel.receive()` returns an arbitrary chunk of the TCP stream (0, partial
  packet, several packets). `IPPacketOutputStream` turns that stream back into
  one-write-per-packet calls on the VPN fd; see
  [04](04-packet-framing.md) for its algorithm.
- `w == -1` is treated as end of stream, but in practice
  `PersistentRelayTunnel.receive()` never returns `-1` (it converts EOF into a
  reconnection attempt); the branch is defensive.

### 2.5 `stop()` — shutdown order matters

```java
public void stop() {
    tunnel.close();               // 1. close the socket => unblocks the receive thread
    tunnelToDeviceFuture.cancel(true);   // 2. interrupt (may break a wait())
    deviceToTunnelFuture.cancel(true);   // 3. interrupt
    wakeUpReadWorkaround();       // 4. unblock the VPN read with a dummy UDP packet
}
```

Why each step:

1. `PersistentRelayTunnel.close()` sets `stopped = true` and invalidates the
   current tunnel. `RelayTunnel.close()` shuts down both streams of the
   `LocalSocket`, which makes any blocked `read()`/`write()` in the *other*
   forwarding thread fail with an `IOException`. That, plus `stopped = true`,
   makes both retry loops exit and throw `InterruptedIOException`.
2. `cancel(true)` interrupts the threads. This is mainly needed when a thread
   is inside `Object.wait()` in the provider's backoff (a blocked socket read
   is not interruptible, but it has already been unblocked by step 1).
3. Same for the send thread.
4. The **only** blocking pair that steps 1–3 cannot break is the
   `vpnInput.read()` in thread 1: neither closing the fd nor interrupting the
   thread wakes up a blocked read on a tun file descriptor. The workaround is
   to make Android deliver *a packet* to the VPN interface, which wakes the
   read; the tunnel being already closed guarantees the packet is never
   forwarded anywhere. It is described in detail in [05](05-edge-cases.md).

### 2.6 `wakeUpReadWorkaround()`

```java
private void wakeUpReadWorkaround() {
    EXECUTOR_SERVICE.execute(() -> {           // network calls not allowed on main thread
        try {
            DatagramSocket socket = new DatagramSocket();
            InetAddress dummyAddr = InetAddress.getByAddress(DUMMY_ADDRESS);
            DatagramPacket packet = new DatagramPacket(new byte[0], 0, dummyAddr, DUMMY_PORT);
            socket.send(packet);
        } catch (IOException e) { /* ignore */ }
    });
}
```

An empty UDP datagram to `42.42.42.42:4242` is routed by the kernel into the
VPN interface (given the default `0.0.0.0/0` route), which satisfies the
pending `read()`. Any failure is ignored: this is best-effort.

## 3. One tunnel connection: `RelayTunnel`

[Source](../../app/src/main/java/com/genymobile/gnirehtet/RelayTunnel.java)

### 3.1 Creating and connecting

```java
public static RelayTunnel open(VpnService vpnService) throws IOException {
    Log.d(TAG, "Opening a new relay tunnel...");
    // since we use a local socket, we don't need to protect the socket from the vpnService anymore
    // but this is an implementation detail, so keep the method signature
    return new RelayTunnel();
}

public void connect() throws IOException {
    localSocket.connect(new LocalSocketAddress(LOCAL_ABSTRACT_NAME)); // "gnirehtet"
    readClientId(localSocket.getInputStream());
}
```

- `LocalSocketAddress("gnirehtet")` uses the **abstract** namespace by default:
  a Linux abstract Unix domain socket, no filesystem entry. It is the endpoint
  that `adbd` created for the `adb reverse` redirection.
- `open()` does not connect; `connect()` is separate because the provider wants
  to hold different locks during creation and during the (slow, blocking)
  connection.
- The `vpnService` parameter is vestigial (kept for signature compatibility
  from the era when `protect()` was needed).

### 3.2 The client-id handshake

```java
private static void readClientId(InputStream inputStream) throws IOException {
    Log.d(TAG, "Requesting client id");
    int clientId = new DataInputStream(inputStream).readInt();
    Log.d(TAG, "Connected to the relay server as #" + Binary.unsigned(clientId));
}
```

As documented in the method's Javadoc: `adb reverse` accept **succeeds even if
no relay is listening** on the host port (and the same false positive exists
for relay-behind-SSH setups). To detect this, the relay immediately writes a
4-byte big-endian client id; the client consumes it *before* declaring the
tunnel connected. Consequences:

- `connect()` returning without exception is a true end-to-end proof of a
  working relay.
- The id is logged for diagnostics (`Binary.unsigned()` because the id is an
  `int` that ordering-wise behaves as a `uint32` on the relay side).
- `DataInputStream.readInt()` blocks until 4 bytes are available or EOF — it
  therefore acts as the "first read" mentioned in the Javadoc.

### 3.3 `send()`, `receive()`, `close()`

```java
public void send(byte[] packet, int len) throws IOException {
    if (GnirehtetService.VERBOSE) Log.v(TAG, "Sending packet: " + Binary.buildPacketString(packet, len));
    localSocket.getOutputStream().write(packet, 0, len);
}

public int receive(byte[] packet) throws IOException {
    int r = localSocket.getInputStream().read(packet);
    if (GnirehtetService.VERBOSE) Log.v(TAG, "Receiving packet: " + Binary.buildPacketString(packet, r));
    return r;
}

public void close() {
    try {
        if (localSocket.getFileDescriptor() != null) {
            // close the streams to interrupt pending read and writes
            localSocket.shutdownInput();
            localSocket.shutdownOutput();
        }
        localSocket.close();
    } catch (IOException e) {
        throw new RuntimeException(e);
    }
}
```

- `send` writes raw packet bytes; **the relay relies on the IP header for
  framing, so chunk boundaries carry no meaning** — no length prefix is added.
- `receive` may return any number of bytes (including `-1` for EOF); framing
  is restored later by `IPPacketOutputStream`.
- `close` does the shutdown-then-close dance precisely so that a thread
  blocked in `read`/`write` on the same socket is unblocked. The failure path
  wraps `IOException` in an unchecked `RuntimeException` ("what could we do?"),
  which propagates through `RelayTunnelProvider.invalidateTunnel()` up to the
  caller — an unlikely but noticeable edge case.
- Verbose logging is compiled in but disabled by `GnirehtetService.VERBOSE =
  false`; `Binary.buildPacketString()` prints at most 20 bytes in hex.

## 4. Reconnection machinery: `RelayTunnelProvider`

[Source](../../app/src/main/java/com/genymobile/gnirehtet/RelayTunnelProvider.java)

The provider owns *the current tunnel* and guarantees that both forwarding
directions use the same one.

```java
private final Object getCurrentTunnelLock = new Object(); // protects getCurrentTunnel()
private final VpnService vpnService;
private final RelayTunnelListener listener;
private RelayTunnel tunnel;            // protected both by "this" and "getCurrentTunnelLock"
private boolean first = true;          // protected by "getCurrentTunnelLock"
private long lastFailureTimestamp;     // protected by "this"
```

### 4.1 `getCurrentTunnel()` — create-or-return, with backoff

```java
public RelayTunnel getCurrentTunnel() throws IOException, InterruptedException {
    synchronized (getCurrentTunnelLock) {
        synchronized (this) {
            if (tunnel != null) {
                return tunnel;
            }
            waitUntilNextAttemptSlot();
            tunnel = RelayTunnel.open(vpnService);
        }
        boolean notifyDisconnectedOnError = first;
        first = false;
        connectTunnel(notifyDisconnectedOnError);
    }
    return tunnel;
}
```

Step by step:

1. **`getCurrentTunnelLock`** serializes *whole calls*. Its raison d'être: the
   two forwarding threads must not each create their own tunnel; also, the
   slow `connect()` must happen exactly once. The Javadoc comment is explicit:
   "to make sure that both the sending and receiving threads use the same
   tunnel, we must guarantee that this method may not be called several times
   concurrently."
2. **Fast path:** if a tunnel already exists, return it immediately.
3. **Backoff:** `waitUntilNextAttemptSlot()` (below), called while holding
   `this` (so that `invalidateTunnel()` can still run — `wait()` releases the
   monitor).
4. **Create, then connect outside the `this` monitor** but still inside the
   call lock. The blocking `connect()` + client-id read deliberately does not
   hold the `this` lock, so invalidation requests can proceed concurrently.
5. **`first` logic:** after the first both-directions join, `first` becomes
   false. On the very first attempt, a connection failure must emit a
   *disconnected* event (the notification already shows the optimistic
   "enabled" state); afterwards, state changes are only notified when they
   actually happen (failures are notified by `invalidateTunnel()`).
6. `return tunnel;` reads the field *outside* the locks; in the unlikely case
   that a concurrent unconditional invalidation (shutdown) nulls it in exactly
   that window, the caller can receive `null`. See [05](05-edge-cases.md)
   for the (benign) details.

### 4.2 Backoff

```java
private static final int DELAY_BETWEEN_ATTEMPTS_MS = 5000;

private void waitUntilNextAttemptSlot() throws InterruptedException {
    if (first) return;                       // first attempt: no wait
    long delay = lastFailureTimestamp + DELAY_BETWEEN_ATTEMPTS_MS - System.currentTimeMillis();
    while (delay > 0) {
        wait(delay);
        delay = lastFailureTimestamp + DELAY_BETWEEN_ATTEMPTS_MS - System.currentTimeMillis();
    }
}
```

- The delay is measured from the **last failure**, not from the last attempt:
  `touchFailure()` is called on every failed connect and on every invalidation.
- The `while` recomputes after each wake-up: spurious wake-ups and failures
  recorded during the sleep extend the wait to 5 s after the *newest* failure.
- `wait(delay)` is called with the `this` monitor held; it releases it while
  sleeping, which is what allows `invalidateTunnel()` to execute concurrently
  (and to move `lastFailureTimestamp` forward).
- No `notifyAll()` is used; waiting threads wake up by timeout. During
  shutdown, `Future.cancel(true)` interrupts the wait, and the resulting
  `InterruptedException` propagates out of `getCurrentTunnel()` — that is why
  the method declares it.

### 4.3 Connecting and notifying

```java
private void connectTunnel(boolean notifyDisconnectedOnError) throws IOException {
    try {
        tunnel.connect();
        notifyConnected();
    } catch (IOException e) {
        touchFailure();
        if (notifyDisconnectedOnError) notifyDisconnected();
        throw e;
    }
}
```

- Success → `notifyConnected()` (main-thread "Reverse tethering enabled").
- Failure → record the failure time (starts the 5 s backoff) and, on the first
  ever attempt, also notify disconnection. The exception is rethrown so the
  caller (`PersistentRelayTunnel`) retries.

> **Subtlety.** On a failed `connect()`, the provider leaves the just-created,
> unconnected `RelayTunnel` in the `tunnel` field (it is only assigned before
> connecting). The caller's local variable is `null`, so it does not invalidate
> it. The next `getCurrentTunnel()` therefore returns that broken tunnel once,
> the immediate I/O fails, *then* the caller invalidates it (with a non-null
> local variable) and the normal 5 s cycle resumes. Net effect: one extra,
> immediate failed attempt after a failed connect. See
> [05](05-edge-cases.md) for the resulting event sequence.

### 4.4 Invalidation

```java
public synchronized void invalidateTunnel() {
    if (tunnel != null) {
        touchFailure();
        tunnel.close();
        tunnel = null;
        notifyDisconnected();
    }
}

public synchronized void invalidateTunnel(Tunnel tunnelToInvalidate) {
    if (tunnel == tunnelToInvalidate || tunnelToInvalidate == null) {
        invalidateTunnel();
    }
}
```

- Invalidation = "this tunnel is dead": close it (unblocking the peer thread),
  clear it, and tell the user.
- The conditional variant exists for the two-forwarding-thread race: thread A
  fails on tunnel *T1* while thread B has already reconnected and installed
  *T2*. A's later invalidation must not kill B's healthy tunnel; hence the
  identity check `tunnel == tunnelToInvalidate`.
- Both methods are `synchronized` on the provider (`this`), which is exactly
  the monitor that `waitUntilNextAttemptSlot()` releases while sleeping — so
  invalidation during a backoff sleep updates `lastFailureTimestamp`
  (via `touchFailure()`) and extends the next wait accordingly, and closes the
  socket so the other direction stops blocking.

### 4.5 The two-lock design, explained

The class has two monitors and a comment explaining why:

| Lock | Protects | Held during |
|------|----------|-------------|
| `getCurrentTunnelLock` | "only one `getCurrentTunnel()` at a time"; `first` | tunnel creation and the blocking `connect()` |
| `this` (the provider object) | `tunnel`, `lastFailureTimestamp`; serializes `invalidateTunnel()` | field reads/writes, `wait()` (released while sleeping) |

Why not a single lock?

- `getCurrentTunnel()` performs potentially slow, blocking I/O. If
  `invalidateTunnel()` had to wait for it, a failing/blocked connect would make
  the peer thread unable to kill the connection (and unable to unblock it with
  `close()`), possibly deadlocking progress until the connect timed out.
- Therefore: one lock (call lock) keeps *creators* exclusive, another lock
  (state lock) lets *destroyers* preempt between atomic field updates. During
  `wait()`, the state lock is released, so invalidation can update the failure
  timestamp while a reconnect is pending.
- The call lock is intentionally **not** released during `connect()`: both
  threads must end up with the same tunnel, so the second caller must wait
  for the outcome. It does not block invalidation, which does not need the
  call lock at all.

### 4.6 Provider event summary

| Trigger | `touchFailure()` | `notifyDisconnected()` | `notifyConnected()` |
|---------|------------------|------------------------|---------------------|
| First connect fails | yes | yes (because `first`) | — |
| Later connect fails | yes | no (already shown as the previous invalidation did it) | — |
| Connect succeeds | — | — | yes |
| `invalidateTunnel()` with a live tunnel | yes | yes | — |
| `invalidateTunnel()` with no tunnel | no | no | — |

## 5. Retry loops: `PersistentRelayTunnel`

[Source](../../app/src/main/java/com/genymobile/gnirehtet/PersistentRelayTunnel.java)

`PersistentRelayTunnel implements Tunnel` and hides reconnection from the
`Forwarder`: to its callers it looks like an infinitely patient tunnel.

```java
private final RelayTunnelProvider provider;
private final AtomicBoolean stopped = new AtomicBoolean();

public void send(byte[] packet, int len) throws IOException {
    while (!stopped.get()) {
        Tunnel tunnel = null;
        try {
            tunnel = provider.getCurrentTunnel();
            tunnel.send(packet, len);
            return;
        } catch (IOException | InterruptedException e) {
            Log.e(TAG, "Cannot send to tunnel", e);
            if (tunnel != null) provider.invalidateTunnel(tunnel);
        }
    }
    throw new InterruptedIOException("Persistent tunnel stopped");
}
```

`receive()` is symmetric, with one difference:

```java
int r = tunnel.receive(packet);
if (r == -1) {                       // EOF = relay closed the connection
    Log.d(TAG, "Tunnel read EOF");
    provider.invalidateTunnel(tunnel);
    continue;                        // reconnect and retry
}
return r;
```

and `close()`:

```java
public void close() {
    stopped.set(true);
    provider.invalidateTunnel();     // unconditional: kills the current tunnel, if any
}
```

Key behaviors:

- **The packet is not lost on failure.** In `send()`, the packet stays in the
  `packet` argument across reconnection attempts: once a new tunnel is ready,
  the same bytes are sent. If the tunnel never comes back, the call blocks
  (after emitting `InterruptedIOException` only when stopped). The device→tunnel
  thread therefore stalls rather than drops — device TCP stacks handle the
  backpressure.
- **Both directions detect and report.** Whichever thread hits the failure
  invalidates the shared tunnel; `RelayTunnel.close()` unblocks the other.
- **EOF is a normal failure mode**, not an end of stream: the relay closes a
  client connection when the relay server is stopped or when the client is
  dropped; the client reconnects.
- **`stopped` is monotonic.** Once `close()` has been called, both loops exit
  with `InterruptedIOException`, which `Forwarder` logs as an expected
  interruption. This is also the mechanism by which `Forwarder.stop()`
  terminates the tunnel-side of both threads.
- **`InterruptedException`** (interruption of the backoff `wait`) is caught the
  same way as an I/O error: invalidate and loop; the loop then sees
  `stopped == true` and exits.

## 6. Event dispatch: `RelayTunnelListener`

[Source](../../app/src/main/java/com/genymobile/gnirehtet/RelayTunnelListener.java)

```java
public class RelayTunnelListener {
    static final int MSG_RELAY_TUNNEL_CONNECTED = 0;
    static final int MSG_RELAY_TUNNEL_DISCONNECTED = 1;
    private final Handler handler;
    public RelayTunnelListener(Handler handler) { this.handler = handler; }
    public void notifyRelayTunnelConnected()    { handler.sendEmptyMessage(MSG_RELAY_TUNNEL_CONNECTED); }
    public void notifyRelayTunnelDisconnected() { handler.sendEmptyMessage(MSG_RELAY_TUNNEL_DISCONNECTED); }
}
```

- It is a thin thread-hop: provider events are emitted on executor threads,
  but `sendEmptyMessage` posts to the main looper of
  `RelayTunnelConnectionStateHandler`, where `Notifier` is updated.
- Messages are idempotent and stateless (no payload). Late messages after
  shutdown are filtered in the handler by `GnirehtetService.isRunning()`.

## 7. End-to-end failure walk-through

A concrete sequence — the relay is stopped while the client is connected and
sending:

```
 t0   relay dies; the TCP connection through adb is closed by the relay side
 t1   thread 2 (device←relay) blocked in LocalSocket read() gets -1 (EOF)
        → PersistentRelayTunnel.receive: invalidateTunnel(current)
            → touchFailure(); RelayTunnel.close() (shuts down both streams)
            → tunnel = null; notifyDisconnected() → main thread → notification:
              "Disconnected from the relay server"
        → loop continues
 t1'  thread 1 (device→relay) blocked in LocalSocket write()/read() gets IOException
        → its own invalidateTunnel(t) is a no-op (t is not current anymore)
        → loop continues
 t2   both threads call getCurrentTunnel(); one wins getCurrentTunnelLock
 t2'  the winner waits until lastFailure + 5 s, creates a tunnel, connects;
        if the relay is still down, connect/read fails:
          touchFailure(); notifyDisconnected() only if this was the very first attempt
          exception → caller loops → one immediate extra attempt on the stale tunnel
          → invalidate → wait another 5 s → retry...
 t3   relay is back: connect() succeeds; the 4-byte client id is read;
        notifyConnected() → notification: "Reverse tethering enabled"
 t4   the blocked peer thread (it was waiting on getCurrentTunnelLock) obtains
      the same new tunnel; both directions resume; thread 1 finally sends the
      packet it had been holding since t0
```

The same flow applies when adb dies: the `LocalSocket` read/write fails the
same way, and reconnection succeeds as soon as `adb reverse` is operational
again.

[← Previous: control plane](02-control-plane.md) · [Index](README.md) · [Next: packet framing →](04-packet-framing.md)
