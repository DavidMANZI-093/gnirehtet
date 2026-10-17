# 01 — Architecture

[← Index](README.md) · [Next: control plane →](02-control-plane.md)

This chapter gives the mental model: what problem the client solves, how it is
plugged into Android, how its pieces collaborate, and where each piece runs.
The remaining chapters zoom into the code.

## 1. The actors

| Actor | Where it runs | Responsibility |
|-------|---------------|----------------|
| Desktop CLI (`gnirehtet` / `gnirehtet.exe`) | The computer | Starts the relay server, installs the APK, sets up `adb reverse`, sends START/STOP intents. |
| `adb` / `adbd` | Both sides | Control channel (`adb shell am start …`) and data channel (`adb reverse`) over USB or TCP. |
| Relay server | The computer | The "brain": parses raw IPv4 packets, maintains real TCP/UDP sockets to the internet, forges response packets. |
| **Client app** | The Android device | The "pipe": captures device traffic through a VPN and pumps raw IPv4 packets to/from the relay. |

A typical invocation:

```bash
./gnirehtet run
# roughly equivalent to:
./gnirehtet relay &                                # relay server on tcp:31416
adb install -r gnirehtet.apk                       # if needed
adb reverse localabstract:gnirehtet tcp:31416      # data tunnel
adb shell am start -a com.genymobile.gnirehtet.START \
    -n com.genymobile.gnirehtet/.GnirehtetActivity # control event
```

Reverse tethering stays active until `Ctrl+C` on the CLI (which sends STOP and
tears down the relay) or the *Stop* action in the device notification.

## 2. Why a VPN?

Android is Linux. To intercept the traffic of *all* apps without root, a
process must receive packets from the kernel network stack. The only public,
non-root way to do that on Android is the `VpnService` API: after explicit user
consent, the app may create a virtual network interface (a *tun* device) and
become the system's default network (or cover specific routes).

The kernel then routes outgoing IPv4 traffic into that interface. The app:

- **reads** from the interface's file descriptor to obtain packets the device
  wants to send;
- **writes** to it to inject packets as if they had arrived from the network.

That is exactly the two halves of the pipe. The relay server, on the computer,
does everything else.

> **Note.** The VPN is not used for privacy here: it merely provides a
> user-space network device. `10.0.0.2/32` is the address assigned to it, and
> by default the route `0.0.0.0/0` sends all IPv4 traffic to it.

## 3. Why the traffic travels over adb

The packets captured by the VPN must reach the computer's relay server. The
client could open a TCP connection to the host, but:

- over USB there is no direct IP connectivity between device and host by
  default;
- `adb reverse` already provides a reliable, ordered, full-duplex byte pipe
  from the device to a port on the host, works over USB and over `adb tcpip`,
  and needs no extra setup.

`adb reverse localabstract:gnirehtet tcp:31416` makes **adbd on the device**
listen on the abstract Unix domain socket `gnirehtet`, and forward every
accepted connection to `tcp:31416` on the host. The app therefore connects a
`LocalSocket` to `gnirehtet` and simply reads/writes bytes. See
[`RelayTunnel`](../../app/src/main/java/com/genymobile/gnirehtet/RelayTunnel.java).

> **Why an abstract Unix socket and not TCP?**
> Historically the client connected to `localhost:31416` and had to call
> `VpnService.protect()` so that the tunnel traffic would *not* be captured by
> the VPN itself (an infinite loop). Since commit `c4a9313` the tunnel uses a
> Unix domain socket, which is not IP traffic and therefore cannot be routed
> into the VPN. The tunnel can never loop on itself, and `protect()` is not
> needed. The unused `vpnService` parameter of `RelayTunnel.open()` is kept
> only because it is "an implementation detail".

## 4. Control plane vs data plane

The client has two clearly separated concerns.

```
             CONTROL PLANE                                 DATA PLANE
  (intents, permission, VPN setup)              (raw packets, background threads)
 ┌───────────────────────────────┐             ┌────────────────────────────────────┐
 │ adb shell am start            │             │  VPN fd  ──Forwarder thread 1──►    │
 │        │                      │             │          PersistentRelayTunnel     │
 │        ▼                      │             │                  │                 │
 │ GnirehtetActivity             │             │            RelayTunnel (socket)    │
 │        │ VpnService.prepare() │             │                  │                 │
 │        ▼                      │             │  VPN fd  ◄─Forwarder thread 2──     │
 │ GnirehtetService              │             │          IPPacketOutputStream      │
 │   ├─ Builder … establish()    │             │                  ▲                 │
 │   ├─ Notifier (start/stop,    │             │            RelayTunnel (socket)    │
 │   │    failure)               │             │                                    │
 │   └─ Forwarder.forward() ─────┼────────────►│  (threads + reconnect machinery)   │
 └───────────────────────────────┘             └────────────────────────────────────┘
       main thread                                  executor threads + main thread
```

| | Control plane | Data plane |
|---|---|---|
| Entry point | `GnirehtetActivity` | `GnirehtetService.startForwarding()` |
| Classes | `GnirehtetActivity`, `GnirehtetService` (setup part), `Notifier`, `VpnConfiguration`, `CIDR`, `Net` | `Forwarder`, `Tunnel`, `RelayTunnel`, `RelayTunnelProvider`, `PersistentRelayTunnel`, `RelayTunnelListener`, `IPPacketOutputStream`, `Binary` |
| Executes on | Main thread | 3 background executor threads + main thread (notification events) |
| Triggered by | `adb` intents / system events | Kernel traffic on the VPN interface, relay traffic |
| Lifetime | Seconds (start/stop events) | Entire duration of the session |

## 5. Packet journeys

### 5.1 Device → internet

1. An app writes to a socket; the kernel picks a route. Because the default
   route points at the VPN interface, the IP packet is delivered to the tun
   device instead of a real NIC.
2. The client's **thread 1** is blocked in `FileInputStream.read()` on the VPN
   file descriptor (`Forwarder.forwardDeviceToTunnel`); it wakes up with the
   packet bytes in its 64 KiB buffer.
3. The code inspects the first nibble of the first byte to make sure this is an
   IPv4 packet. Anything else (in particular IPv6) is logged and dropped —
   the relay does not support it.
4. `PersistentRelayTunnel.send()` obtains the current `RelayTunnel` from the
   provider (reconnecting with a 5-second backoff if needed) and writes the
   packet to the `LocalSocket` output stream.
5. adbd forwards the bytes over the adb connection; the relay server reads them
   as a byte stream.
6. The relay parses packet boundaries, looks up or creates the matching
   connection (protocol, source/destination address and port), and performs
   ordinary socket I/O toward the internet.

### 5.2 Internet → device

1. The relay receives data from a real socket.
2. It forges a raw IPv4 response packet: original IP/transport headers copied
   with source and destination swapped, payload appended, checksums computed
   (`Packetizer` + `IPv4Header.swapSourceAndDestination()` in the relay).
3. The packet is written to the client's TCP connection — a *byte stream*, so
   the client may read half a packet, three packets, or a packet split across
   several reads.
4. The client's **thread 2** (`Forwarder.forwardTunnelToDevice`) reads whatever
   is available and feeds it to `IPPacketOutputStream`.
5. `IPPacketOutputStream` buffers and analyzes the stream: it reads the IP
   version and total length from the header, and writes **exactly one packet
   per `write()` call** to the VPN file descriptor (the tun device requires
   one write per packet).
6. The kernel accepts each packet as if it came from the network and delivers
   it to the destination app — the response's destination address is the
   device's own `10.0.0.2`, because the relay swapped the addresses of the
   request.

## 6. Process and threading model

Everything runs in a single app process, with one main thread and a small
fixed thread pool.

| Thread | Created by | Runs |
|--------|------------|------|
| Main (UI) | Android | `GnirehtetActivity`, `GnirehtetService` callbacks (`onStartCommand`), `Notification` updates, `Handler` messages from `RelayTunnelListener` |
| `pool-1-thread-1` | `Forwarder.EXECUTOR_SERVICE` (`Executors.newFixedThreadPool(3)`) | `forwardDeviceToTunnel()` — VPN → relay (blocking) |
| `pool-1-thread-2` | same | `forwardTunnelToDevice()` — relay → VPN (blocking) |
| `pool-1-thread-3` | same | transient: the wake-up datagram sent when stopping (see [05](05-edge-cases.md)) |

Notes:

- The executor is **static**: it is shared by all `Forwarder` instances in the
  process (in practice there is only one at a time).
- The two loops are separate threads, so they can block independently. Both
  share the *same* `PersistentRelayTunnel` / `RelayTunnel` instance; the
  provider synchronizes their access (see [03](03-data-plane.md)).
- Tunnel state changes are *produced* on these pool threads but *consumed* on
  the main thread: `RelayTunnelListener` wraps a `Handler` bound to the main
  looper, so `Notifier.setFailure()` is always called from the main thread.
- Android forbids network operations on the main thread (`NetworkOnMainThreadException`);
  anything touching a socket or the network runs on the pool, including the
  wake-up workaround in `Forwarder.stop()`.

## 7. State machines

### 7.1 Client session (service)

```
                START intent (adb)                 journal:
      ┌───────────────────────────────┐
      ▼                               │
 [VPN idle] ── first time ──► system consent dialog ── yes ──► [VPN running]
      │                               │                          │
      │                               └─ no ──► [VPN idle]       │
      │                                                          │
      └──────────────── STOP intent ◄────────────────────────────┘
                              (or notification Stop action)
```

While running, a second START intent is ignored ("VPN already running"); a STOP
when idle is ignored ("already closed").

### 7.2 Relay tunnel

The tunnel state is not exposed as a public enum; it is the combination of
`RelayTunnelProvider.tunnel` and the last notification event.

```
                       connect() + read client id OK
   [no tunnel] ───────────────────────────────────────► [connected]
        ▲  │                                                │  │
        │  │ connect()/read fails                          │  │ I/O error, EOF,
        │  ▼                                                 │  │ or tunnel.close()
   [wait 5 s] ◄──────────── invalidate ◄───────────────────┘  │
        ▲                                                      │
        └────────────────────── invalidate ◄───────────────────┘
```

- One `RelayTunnel` instance is shared by both directions.
- On any I/O failure, the tunnel is invalidated (closed) and both directions
  re-enter `getCurrentTunnel()`. The first to arrive reconnects; the other
  blocks and then joins the fresh tunnel.
- The 5-second delay (`DELAY_BETWEEN_ATTEMPTS_MS`) is counted from the **last
  failure**, not from the last attempt.

### 7.3 Notification state

```
notifier.start()          tunnel connected        tunnel disconnected
 connected icon  ─────────►  connected icon  ─────►  failure icon + text
 "Reverse tethering enabled"  … (reset false) …      "Disconnected from the relay server"
```

Events arriving after `close()` are ignored if the VPN is no longer running
(obsolete `Handler` messages).

## 8. The wire protocol, precisely

```
   client connects over adb reverse
        │
        ├─────────────────────────────────────────────►
        │   (relay) accepts, then immediately writes:
        │       4 bytes: client id, big-endian uint32
        │◄─────────────────────────────────────────────
   client reads the 4 bytes with DataInputStream.readInt()
   → logs "Connected to the relay server as #<id>"
        │
        │   then, both directions, forever:
        │
        ├──── device → relay : raw IPv4 packets ────────►
        │◄─── relay → device : raw IPv4 packets ─────────
```

- The 4-byte id doubles as a **readiness handshake**. `adb reverse` accepts
  connections even when nothing listens on the host port (the redirection is
  local to adbd), so `connect()` succeeds vacuously; the first read is what
  actually fails. Receiving the id guarantees an end-to-end path, including
  setups where the relay is behind an SSH port forward.
- There is **no length prefix or message boundary** on the packet stream. In
  the relay→device direction, `IPPacketOutputStream` re-frames using the IPv4
  header. In the device→relay direction, chunk boundaries written by the
  client do not matter; the relay applies the same stream parsing.
- Only IPv4 is supported end to end. No IPv6, no ICMP (the relay supports TCP
  and UDP payloads only; ICMP packets are logged/dropped by the relay, and the
  client does not generate any).

## 9. Boundaries, trust and Android security

- **Starting/stopping requires shell privileges.** `GnirehtetActivity` is
  `exported="true"` but protected by
  `android:permission="android.permission.WRITE_SECURE_SETTINGS"`. Ordinary
  apps do not hold that permission; `adb shell` does. This is how the tool
  ensures only the computer (via adb) can control the client.
- **VPN consent is the user's.** The system dialog (`VpnService.prepare()`)
  must be accepted once; without it `establish()` returns `null`.
- **No custom protocol security.** The packet stream is not encrypted by the
  app; confidentiality/authenticity, if needed, comes from the adb/USB channel
  itself. Over `adb tcpip` (adb over Wi-Fi) traffic may travel over the
  network in cleartext.
- **No persistence.** The app stores nothing (`allowBackup="false"`), has no
  settings and no launcher icon.

## 10. Failure domains

| Failure | What the client does | User-visible effect |
|---------|----------------------|---------------------|
| Relay server stopped/restarted | I/O error or EOF on the socket → invalidate → retry every 5 s | Notification flips to "Disconnected…", then back when reconnection succeeds |
| adb connection dropped | Same (socket dies) | Same; reconnect succeeds as soon as adb is back |
| VPN permission revoked | Process is stopped by the system; nothing special is handled | No internet through gnirehtet; must re-run `gnirehtet start` |
| Client process killed | Nothing (service is `START_NOT_STICKY`) | Tunnel gone; user must re-run |
| Computer has no internet | Nothing — the app cannot know; the relay just fails to open sockets | Apps see connection timeouts; notification still says "enabled" |
| Device sends IPv6 | Packet dropped by the client | IPv6-only apps do not work (known limitation) |
| Device's custom routes exclude `42.42.42.42` | Stop-time wake-up packet may not wake the blocking read (see [05](05-edge-cases.md)) | `stop` may leave a forwarding thread blocked; process usually dies anyway |

[← Index](README.md) · [Next: control plane →](02-control-plane.md)
