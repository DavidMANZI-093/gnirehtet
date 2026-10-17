# 04 — Packet framing: raw IPv4 over a byte stream

[← Previous: data plane](03-data-plane.md) · [Index](README.md) · [Next: edge cases →](05-edge-cases.md)

The courier between the device and the relay is TCP-like: it guarantees bytes,
order, and integrity, but **not message boundaries**. The VPN interface, on
the other hand, is packet-oriented: every `write()` must contain exactly one
IP packet, and every `read()` returns exactly one. This chapter explains how
the mismatch is resolved with `IPPacketOutputStream`, and what `Binary` adds.

## 1. The problem

Two directions, two different framing situations:

| Direction | What the sender does | What the receiver sees |
|-----------|----------------------|------------------------|
| Device → relay | `Forwarder` reads one packet per `read()` and writes it to the socket as-is | The relay's stream parser relies on the IP header; chunk boundaries do not matter |
| Relay → device | The relay writes packet after packet into the TCP stream (coalescing is possible and happens) | The client may read N bytes that contain a partial packet, a full packet, several packets, or any combination |

So the only place where framing must be *reconstructed* is the relay→device
direction, just before writing to the tun fd. That is the job of
`IPPacketOutputStream`.

> **Why is framing even needed?** `VpnService`'s file descriptor is a tun
> device: a write corresponds to injecting one IP packet into the network
> stack, and the kernel determines the packet length from the datagram. If you
> wrote two packets in one `write()`, the kernel would treat them as one
> malformed packet. Hence one packet per write.

## 2. The fields that make it possible

Framing needs no extra header — each IPv4 packet describes itself (see
[IPv4 header](https://en.wikipedia.org/wiki/IPv4#Header)):

```
 byte 0             byte 1                 bytes 2-3
┌────────┬────────┐┌────────┬────────────┐┌────────────────┐
│ version│  IHL   ││   DSCP │   ECN      ││  total length  │
│  4 b   │  4 b   ││ 6 b    │   2 b      ││   16 b (BE)    │
└────────┴────────┘└────────┴────────────┘└────────────────┘
   = 4 for IPv4                              header + payload
```

- **Version**: high nibble of byte 0 (`0x40` for IPv4). A leading byte whose
  high nibble is not 4 cannot be the start of a supported packet.
- **Total length**: unsigned 16-bit big-endian value at offset 2 — the length
  of the whole packet, header included. This is what tells the client how many
  bytes to accumulate before performing a `write()`.

`MAX_IP_PACKET_LENGTH` is therefore `1 << 16` (65536 = one more than the
largest representable value, used as an array bound).

## 3. `IPPacketOutputStream` internals

[Source](../../app/src/main/java/com/genymobile/gnirehtet/IPPacketOutputStream.java)

```java
public class IPPacketOutputStream extends OutputStream {
    private static final int MAX_IP_PACKET_LENGTH = 1 << 16; // 65536
    private final OutputStream target;                        // the VPN fd stream
    // must always accept 1 full packet + any partial packet
    private final ByteBuffer buffer = ByteBuffer.allocate(2 * MAX_IP_PACKET_LENGTH);
```

### 3.1 The buffer invariant

After every public `write(...)` call, `sink()` has drained all *complete*
packets, so the buffer keeps **at most one partial packet** (length ≤ 65535).
The next call can add at most one full chunk of ≤ 65536 bytes. Worst case:
65535 + 65536 = 131071 < 131072. The 2× buffer size is thus exactly what the
"one full packet + any partial packet" comment claims, and a single `put` can
never overflow.

### 3.2 `write(byte[], int, int)` — the path used by the `Forwarder`

```java
public void write(byte[] b, int off, int len) throws IOException {
    if (len > MAX_IP_PACKET_LENGTH) {
        throw new IOException("IPPacketOutputStream does not support writing more than one packet at a time");
    }
    if (BuildConfig.DEBUG && len > buffer.remaining()) {
        Log.e(TAG, len + " must be <= than " + buffer.remaining());
        Log.e(TAG, buffer.toString());
        throw new AssertionError("Buffer is unexpectedly full");
    }
    buffer.put(b, off, len);   // append the chunk at the current position
    buffer.flip();             // readable region = [position, limit)
    sink();                    // emit every complete packet…
    buffer.compact();          // …then move any leftover partial packet to offset 0
}
```

- The `len > MAX` guard is an API contract: a caller must not present more
  bytes than one maximum-size packet. In practice `Forwarder` passes what
  `tunnel.receive()` returned (≤ 65536), never more.
- The overflow check is a **debug-only** assertion (`BuildConfig.DEBUG`), not
  a runtime contract: it exists to catch a violated invariant during
  development.
- `flip()` + `sink()` + `compact()` is the standard "write into a partially
  filled buffer" idiom: the readable region starts where the leftover began,
  and compaction preserves the leftover for the next call.

### 3.3 `write(int)` — single byte

Same idea, but it fails with `IOException` when the buffer has no room
(the theoretical only case would be a 131072-byte partial packet, which cannot
happen because `sinkPacket()` would have emitted anything ≥ 65536 bytes long).
Unused by the forwarding loop, kept for `OutputStream` completeness.

### 3.4 `sink()` / `sinkPacket()` — extracting packets

```java
private void sink() throws IOException {
    while (sinkPacket()) { /* continue */ }
}

private boolean sinkPacket() throws IOException {
    int version = readPacketVersion(buffer);
    if (version == -1) return false;                       // nothing buffered

    if (version != 4) {
        Log.e(TAG, "Unsupported packet received, IP version is:" + version);
        Log.d(TAG, "Clearing buffer");
        buffer.clear();                                    // drop everything
        return false;
    }

    int packetLength = readPacketLength(buffer);
    if (packetLength == -1 || packetLength > buffer.remaining()) {
        return false;                                      // incomplete: wait for more bytes
    }

    target.write(buffer.array(), buffer.arrayOffset() + buffer.position(), packetLength);
    buffer.position(buffer.position() + packetLength);
    return true;
}
```

Decision table:

| Situation | `readPacketVersion` | `readPacketLength` | Action |
|-----------|--------------------|--------------------|--------|
| Buffer empty | `-1` | not called | Stop; nothing to do |
| First byte high nibble ≠ 4 | version ≠ 4 | not called | Error log, **clear the whole buffer**, stop |
| Fewer than 4 bytes available | 4 | `-1` | Stop; wait for the length field |
| Length field present but packet incomplete | 4 | value > remaining | Stop; wait for the rest |
| Complete packet | 4 | value ≤ remaining | Write exactly `packetLength` bytes to the target, advance, try the next one |

The two helpers read *absolute* positions (`buffer.get(index)` /
`buffer.getShort(index)`) relative to `position()`, so they always analyze the
packet sitting at the front of the readable region:

```java
public static int readPacketVersion(ByteBuffer buffer) {
    if (!buffer.hasRemaining()) return -1;
    byte versionAndIHL = buffer.get(buffer.position());
    return (versionAndIHL & 0xf0) >> 4;
}

public static int readPacketLength(ByteBuffer buffer) {
    if (buffer.limit() < buffer.position() + 4) return -1;  // not even the length field
    return Binary.unsigned(buffer.getShort(buffer.position() + 2)); // unsigned 16-bit
}
```

### 3.5 What the "clear the buffer" decision means

A non-IPv4 byte sequence at the head of the stream cannot be parsed or
re-synchronized: there is no trustworthy length information. `buffer.clear()`
throws away every buffered byte (including any well-formed packets that
followed the garbage). It is a defensive refusal to play along with data the
relay protocol should never produce. In practice the relay only ever sends
IPv4. (Also note: the client drops IPv6 coming *from the device* in
`Forwarder`, and the relay never echoes it — see [05](05-edge-cases.md).)

> **Robustness note.** A packet whose advertised total length is `0` would
> make `sinkPacket()` return `true` without consuming bytes, i.e. an infinite
> loop. This is not guarded, on the assumption that the relay computes the
> length from real data and never emits `0`. Something to keep in mind if the
> protocol is ever exposed to untrusted parties.

## 4. Worked examples (from the unit tests)

The repository has one test class for this subsystem,
[`TestIPPacketOutputSteam`](../../app/src/test/java/com/genymobile/gnirehtet/TestIPPacketOutputSteam.java)
(note the typo in the class name). It builds a synthetic 32-byte UDP-over-IPv4
packet whose `total length` field says 32:

```
0x45        version/IHL       (IPv4, IHL 5)
0x00        DSCP/ECN
0x00 0x20   total length = 32  ← framing field
…           other IP fields, UDP header, payload
```

### 4.1 One packet fed in fragments

```java
pos.write(rawPacket, 0, 14);   // incomplete → target receives 0 bytes
pos.write(rawPacket, 14, 14);  // still incomplete → 0 bytes
pos.write(rawPacket, 28, 4);   // packet complete → 32 bytes written, exactly once
```

Asserts: nothing is written until the packet is complete, then the bytes are
identical to the original packet.

### 4.2 Several packets in one chunk

Three 32-byte packets concatenated (96 bytes total):

```java
pos.write(rawPackets, 0, 70);   // 2 complete packets (64 bytes) + 6 bytes
// → target.size() == 64 and target received *2 separate write calls*
pos.write(rawPackets, 70, 26);  // finishes packet 3
// → target.size() == 96 and target received *3 write calls* total
```

The capturing stream counts `write` invocations to prove that packets are
written **individually** — which is precisely the property the tun device
requires.

Run them with:

```bash
./gradlew :app:testDebugUnitTest        # or: ./gradlew :app:test
```

## 5. The other direction, and why no framing is added

Device → relay does not need an encoder:

- `read()` on the tun fd returns exactly one packet; the `Forwarder` writes
  those bytes to the socket in one `send()`.
- Even if the read returned several packets (it does not), the relay's stream
  parser (`IPv4PacketBuffer` / `Ipv4PacketBuffer` in the relay) would
  reconstruct boundaries the same way the client does: version + total length.
- The **only** check the client performs is the version test on the first
  byte, to drop non-IPv4 packets before they hit the wire.

So the protocol is symmetric in spirit — *the IP header is the framing* — but
only the relay→device side needs an explicit deframer because it is glued to a
packet-oriented fd.

## 6. MTU and why it is 0x4000

`GnirehtetService.setupVpn()` sets the interface MTU to `0x4000` (16384 bytes)
with this comment:

```java
// magic value: higher (like 0x8000 or 0xffff) or lower (like 1500) values show poorer performances
private static final int MTU = 0x4000;
```

Context (commit `4ab392e`, "Reduce CPU usage by tweaking MTU value"): gnirehtet
used to forge packets of at most ~1440 bytes, and the host CPU was dominated by
per-packet overhead in adb and in the relay. Larger packets amortize that cost.
The author measured that increasing the VPN MTU reduced host CPU dramatically,
but that pushing it to 0x8000/0xFFFF (or down to 1500) made performance *worse*
for reasons not fully understood — hence the empirically chosen 0x4000.

The MTU also bounds what the `Forwarder`'s 64 KiB buffers ever see in
practice: packets read from the tun are ≤ 16384 bytes, and the relay *also*
hardcodes the same MTU (`relay-java/.../TCPConnection.java` and
`relay-rust/src/relay/tcp_connection.rs`, both commented "same value as
GnirehtetService.MTU in the client") to cap the TCP payload it reads, so
forged response packets fit the device's MTU as well. The buffers are still
sized for the theoretical IPv4 maximum so that the framing logic never depends
on the MTU.

## 7. `Binary` — small helpers

[Source](../../app/src/main/java/com/genymobile/gnirehtet/Binary.java)

| Method | Purpose |
|--------|---------|
| `unsigned(byte)` / `unsigned(short)` / `unsigned(int)` | Convert signed Java values to unsigned `int`/`long` (used for lengths, ids, and packet hex dumps) |
| `buildPacketString(byte[] data, int len)` | Format up to 20 bytes as hex, grouped 4 bytes at a time, with `[len bytes]` header and `... +N bytes` suffix when truncated |

`buildPacketString` is only used behind the `GnirehtetService.VERBOSE` flag
(`false` in release) for "Sending/Receiving packet" logs in `RelayTunnel`. It
makes `adb logcat` a usable packet debugger when enabled:

```
D RelayTunnel: Sending packet: [32 bytes] 45 00 00 20  00 00 00 00  00 11 00 00  0A 00 00 02  08 08 08 08 ...
```

## 8. Summary of sizes and constants

| Constant | Value | Where | Meaning |
|----------|-------|-------|---------|
| `BUFSIZE` | `0x10000` (65536) | `Forwarder` | read buffer both directions |
| `MAX_IP_PACKET_LENGTH` | `1 << 16` (65536) | `IPPacketOutputStream` | bound of one packet; buffer = 2× |
| `MTU` | `0x4000` (16384) | `GnirehtetService` | VPN interface MTU |
| `MAX_STRING_PACKET_SIZE` | 20 | `Binary` | hex dump truncation for verbose logs |
| IP total length field | 16 bits | IPv4 header | the framing value (max 65535) |

[← Previous: data plane](03-data-plane.md) · [Index](README.md) · [Next: edge cases →](05-edge-cases.md)
