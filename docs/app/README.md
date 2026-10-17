# Gnirehtet Android client — documentation (`app/`)

> **Scope.** This directory documents the Android application in
> [`app/`](../../app) — the **client** side of _Gnirehtet_. It explains what the
> client does, how it is wired to the Android platform and to the relay server,
> and why the code is written the way it is.
>
> **Version.** Gnirehtet 2.5.1 (commit `1eb2e58`). The code is the source of
> truth; this documentation is a reading guide for it.

## Contents

| Document | What it covers |
|----------|----------------|
| [01 — Architecture](01-architecture.md) | The big picture: where the app fits, control plane vs data plane, process/threading model, state machines, packet journeys, wire protocol |
| [02 — Control plane](02-control-plane.md) | How the app is started and stopped: intents, `GnirehtetActivity`, `VpnConfiguration`, `GnirehtetService`, VPN setup, the foreground `Notifier` |
| [03 — Data plane](03-data-plane.md) | How packets flow: `Forwarder`, `Tunnel`/`RelayTunnel`, the persistent reconnecting tunnel (`RelayTunnelProvider`, `PersistentRelayTunnel`), locking and failure handling |
| [04 — Packet framing](04-packet-framing.md) | How raw IPv4 packets are carried over a TCP byte stream and re-framed for the VPN interface: `IPPacketOutputStream`, `Binary`, MTU choices, unit tests |
| [05 — Edge cases & quirks](05-edge-cases.md) | Workarounds and corner cases: blocking-read wake-up, connect detection, non-IPv4 packets, notification races, service lifetime, known limitations |
| [06 — Class reference](06-class-reference.md) | One-place summary of every class, plus manifest, resources, build configuration, and an extension guide |

## 1. What the client is

_Gnirehtet_ lets an Android device use the internet connection of the computer
it is plugged into (**reverse tethering**), without root, on either side. It
consists of:

- a **relay server** running on the computer (Java or Rust, in
  [`relay-java/`](../../relay-java) and [`relay-rust/`](../../relay-rust));
- this **Android client**, an APK with *no UI*;
- **adb**, which provides both the command channel to the device and the data
  tunnel between the device and the relay server.

The client is an extremely small program with one job:

> **Capture every IPv4 packet the device wants to send out, ship it to the
> relay server over the adb connection, and inject the packets sent back into
> the device's network stack.**

The client does **not** understand TCP or UDP. It does not terminate
connections, does not implement retransmission, DNS, or NAT. All of that "hard
work" happens in the relay server on the computer, which opens real sockets to
the internet on behalf of the device. From the app's point of view it is a
**dumb pipe** for raw IP datagrams.

## 2. End-to-end path

```
   ANDROID DEVICE                                              COMPUTER (host)
┌─────────────────────────────────────────┐   ┌────────────────────────────────────────┐
│ Android apps & system services          │   │                                        │
│        │ ordinary sockets               │   │                                        │
│        ▼                                │   │                                        │
│ Linux network stack                     │   │    relay server                        │
│   route 0.0.0.0/0 → VPN interface       │   │    (relay-java / relay-rust)           │
│        │                                │   │    listening on tcp:31416              │
│        ▼                                │   │           │ real TCP/UDP sockets       │
│ VPN interface 10.0.0.2/32 (tun)         │   │           ▼                            │
│        │ read()/write()                 │   │        Internet                        │
│        ▼                                │   │                                        │
│ Gnirehtet client app                    │   │                                        │
│        │ raw IPv4 packets               │   │                                        │
│        ▼                                │   │                                        │
│ LocalSocket (abstract "gnirehtet")      │   │                                        │
│        │                                │   │                                        │
│ adbd ───┼───── adb reverse ─────────────┼───┼─►  tcp:31416                          │
└─────────┴───────────────────────────────┘   └────────────────────────────────────────┘
```

`adb reverse localabstract:gnirehtet tcp:31416` makes `adbd` on the device
listen on the *abstract* Unix socket `gnirehtet`; every byte written to that
socket is tunneled over USB (or adb-over-TCP) to port 31416 on the host, where
the relay server listens. The app therefore talks to the relay by opening a
single `LocalSocket`.

The two directions of the pipe:

- **device → internet:** an app sends a packet → kernel routes it into the VPN
  interface → the client reads it → writes it to the local socket → relay
  receives it, opens/uses a real socket → internet.
- **internet → device:** relay receives data on a real socket → forges a raw
  IPv4 packet with the addresses/ports swapped → writes it to the TCP stream →
  client re-splits the stream into packets → writes them to the VPN interface →
  the kernel delivers them to the app.

See [01 — Architecture](01-architecture.md) for the full walkthrough.

## 3. What the client is responsible for

1. **Receive the control intents.** `adb shell am start -a
   com.genymobile.gnirehtet.START` (or `.STOP`), with optional DNS/routes
   extras. Handled by [`GnirehtetActivity`](../../app/src/main/java/com/genymobile/gnirehtet/GnirehtetActivity.java),
   an invisible activity (modern Android refuses to start services directly
   from `adb`).
2. **Obtain VPN consent.** The first time, Android shows a system dialog; the
   activity must be there to show it, so it uses `startActivityForResult` and
   keeps itself alive until the user answers.
3. **Become the default network.** The app asks the system to create a tun
   interface (`10.0.0.2/32`, MTU `0x4000`, route `0.0.0.0/0` by default), which
   captures all IPv4 traffic. This is done by [`GnirehtetService`](../../app/src/main/java/com/genymobile/gnirehtet/GnirehtetService.java),
   a `VpnService`.
4. **Move packets both ways.** [`Forwarder`](../../app/src/main/java/com/genymobile/gnirehtet/Forwarder.java)
   runs two blocking loops on background threads: VPN file descriptor ↔ relay
   tunnel.
5. **Stay connected.** The relay may be (re)started or the adb tunnel may drop
   while the client keeps running; [`PersistentRelayTunnel`](../../app/src/main/java/com/genymobile/gnirehtet/PersistentRelayTunnel.java)
   transparently reconnects every 5 seconds and keeps the VPN alive meanwhile.
6. **Report status.** A foreground notification (mandatory on Android O+)
   shows "Reverse tethering enabled" or "Disconnected from the relay server"
   and offers a *Stop* action.

What it does **not** do: no UI, no settings, no IPv6 (IPv6 packets are
discarded), no local network configuration beyond the VPN interface, no traffic
when no relay is reachable (packets are held/retried rather than silently
dropped, see [05](05-edge-cases.md)).

## 4. Component map

| Class | Role | Details |
|-------|------|---------|
| `GnirehtetActivity` | Invisible entry point; parses START/STOP intents, asks for VPN consent | [02](02-control-plane.md) |
| `GnirehtetService` | `VpnService`; owns the VPN interface and the `Forwarder`; shows the notification | [02](02-control-plane.md) |
| `Notifier` | Foreground-service notification and its failure/stop states | [02](02-control-plane.md) |
| `VpnConfiguration` | Parcelable bundle of DNS servers and routes passed in intents | [02](02-control-plane.md) |
| `CIDR`, `Net`, `InvalidCIDRException` | Parsing/conversion helpers for addresses and routes | [02](02-control-plane.md) |
| `Forwarder` | The two pumping loops between the VPN interface and the tunnel | [03](03-data-plane.md), [04](04-packet-framing.md) |
| `Tunnel` | Blocking packet transport interface | [03](03-data-plane.md) |
| `RelayTunnel` | One `LocalSocket` connection to the relay, including the client-id handshake | [03](03-data-plane.md) |
| `RelayTunnelProvider` | Creates/caches/replaces the current tunnel; 5 s reconnect backoff | [03](03-data-plane.md) |
| `PersistentRelayTunnel` | Retry loops around the provider; implements `Tunnel` | [03](03-data-plane.md) |
| `RelayTunnelListener` | Thread-hop for connected/disconnected events toward the main thread | [03](03-data-plane.md) |
| `IPPacketOutputStream` | Re-frames a TCP byte stream into individual IP packets for the VPN fd | [04](04-packet-framing.md) |
| `Binary` | Unsigned conversions and hex packet dumps (verbose logs) | [04](04-packet-framing.md) |
| `BuildConfig` | Generated: `DEBUG` switch used by a debug assertion | [04](04-packet-framing.md) |

## 5. Wire protocol in three bullets

1. **Transport:** one reliable, ordered byte stream per device — a Unix domain
   socket on the device side, TCP `localhost:31416` on the host side, glued by
   `adb reverse`.
2. **Handshake:** as soon as the relay accepts a connection it writes the
   client id as a **4-byte big-endian integer**. The client reads (and logs)
   it before considering the tunnel connected. This detects the very common
   false positive where `adb reverse` accepts the connection although no relay
   is listening on the host.
3. **Payload:** full-duplex stream of **raw IPv4 packets**, no additional
   framing. Packet boundaries are recovered from the IP header itself (version
   in the first nibble, total length in bytes 2–3), chunked arbitrarily by TCP
   in the relay→device direction.

## 6. Glossary

| Term | Meaning |
|------|---------|
| **Client** | The Android app in [`app/`](../../app) — this documentation's subject. |
| **Relay (server)** | The program on the computer that terminates TCP/UDP and talks to the internet. |
| **Reverse tethering** | Using the computer's internet connection *for* the device (the reverse of USB tethering). |
| **VPN interface / tun** | The virtual network device created by `VpnService.establish()`; every packet written to it appears to have arrived from the network, every read from it is a packet the device wanted to send. |
| **Device→tunnel** | Data direction: packet read from the VPN fd, sent to the relay. |
| **Tunnel→device** | Data direction: packet received from the relay, written to the VPN fd. |
| **`adb reverse`** | `adb` feature that lets a socket on the device be served by a socket on the host; here: abstract `gnirehtet` → host `tcp:31416`. |
| **Client id** | Integer assigned by the relay to each connected client; used by the client only as a connectivity proof (logged for debugging). |

## 7. Reading order

1. [01 — Architecture](01-architecture.md): read first for the big picture.
2. [02 — Control plane](02-control-plane.md): how the app starts, gets
   permission, and configures the VPN.
3. [03 — Data plane](03-data-plane.md): the packet pump and the reconnect
   machinery — the heart of the client.
4. [04 — Packet framing](04-packet-framing.md): the byte-level details of
   packet transport.
5. [05 — Edge cases & quirks](05-edge-cases.md): everything that can go wrong,
   and the workarounds baked into the code.
6. [06 — Class reference](06-class-reference.md): lookup tables when you know
   *what* you are looking for but not *where*.
