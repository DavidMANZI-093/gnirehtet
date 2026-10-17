# 06 — Class reference, manifest, build

[← Previous: edge cases](05-edge-cases.md) · [Index](README.md)

Lookup-oriented summary. The narrative chapters ([02](02-control-plane.md),
[03](03-data-plane.md), [04](04-packet-framing.md)) explain *why*; this is the
*what/where*.

## 1. File map

```
app/
├── build.gradle                     Android application module
├── proguard-rules.pro               (unused: minifyEnabled = false)
└── src/
    ├── main/
    │   ├── AndroidManifest.xml
    │   ├── java/com/genymobile/gnirehtet/
    │   │   ├── GnirehtetActivity.java        invisible entry point (intents)
    │   │   ├── GnirehtetService.java         VpnService, session owner
    │   │   ├── Notifier.java                 foreground notification
    │   │   ├── VpnConfiguration.java         parcelable config
    │   │   ├── CIDR.java                     address + prefix
    │   │   ├── InvalidCIDRException.java     parse error
    │   │   ├── Net.java                      address/CIDR helpers
    │   │   ├── Forwarder.java                VPN fd ↔ tunnel pump
    │   │   ├── Tunnel.java                   blocking transport interface
    │   │   ├── RelayTunnel.java              one LocalSocket connection
    │   │   ├── RelayTunnelProvider.java      current tunnel + reconnect
    │   │   ├── PersistentRelayTunnel.java    retry loops (implements Tunnel)
    │   │   ├── RelayTunnelListener.java      events → main-thread Handler
    │   │   ├── IPPacketOutputStream.java     stream → packets framing
    │   │   └── Binary.java                   unsigned/hex helpers
    │   └── res/
    │       ├── drawable/ic_close_24dp.xml
    │       ├── drawable/ic_report_problem_24dp.xml
    │       ├── drawable/ic_usb_24dp.xml
    │       ├── values/strings.xml
    │       ├── values-fr/strings.xml
    │       └── values/styles.xml
    └── test/java/com/genymobile/gnirehtet/
        └── TestIPPacketOutputSteam.java      framing unit tests
```

## 2. Classes

### 2.1 `GnirehtetActivity`

| Member | Contract |
|--------|----------|
| `ACTION_GNIREHTET_START` / `_STOP` | Intent actions `com.genymobile.gnirehtet.START` / `.STOP`. |
| `EXTRA_DNS_SERVERS` / `EXTRA_ROUTES` | `String[]` extras, keys `dnsServers` / `routes`. |
| `VPN_REQUEST_CODE = 0` | Request code for the system consent dialog. |
| `onCreate` | Delegates to `handleIntent(getIntent())`; no UI. |
| `handleIntent` | Dispatches START/STOP; decides whether the activity may finish (no, while a consent dialog is pending). |
| `createConfig(Intent)` | Builds a `VpnConfiguration` from extras (null-safe → empty arrays). |
| `startGnirehtet(VpnConfiguration)` | `VpnService.prepare()`; if already authorized, starts the service and returns `true`; else shows the dialog and returns `false`. |
| `stopGnirehtet()` | `GnirehtetService.stop(this)`. |
| `requestAuthorization(Intent, VpnConfiguration)` | Saves the config field and calls `startActivityForResult`. |
| `onActivityResult` | On `RESULT_OK`, starts the service with the saved config; always finishes. |

### 2.2 `GnirehtetService`

| Member | Contract |
|--------|----------|
| `VERBOSE = false` | Enables per-packet hex logs in `RelayTunnel` (recompile). |
| `ACTION_START_VPN` / `ACTION_CLOSE_VPN` | Internal service actions `com.genymobile.gnirehtet.START_VPN` / `.CLOSE_VPN`. |
| `EXTRA_VPN_CONFIGURATION` | `Parcelable` config extra. |
| `VPN_ADDRESS` | `10.0.0.2` (`Net.toInetAddress(new byte[]{10,0,0,2})`). |
| `MTU = 0x4000` | VPN interface MTU (tuned value). |
| `start(Context, VpnConfiguration)` | Builds the start intent; `startForegroundService` on O+, `startService` before. |
| `stop(Context)` | Builds the stop intent with the same API-level logic. |
| `createStopIntent(Context)` | Stop intent factory, shared with the notification action. |
| `onStartCommand` | START_VPN → ignore if running, else `startVpn(config or default)`; CLOSE_VPN → `close()`; returns `START_NOT_STICKY`. |
| `isRunning()` | `vpnInterface != null`. |
| `startVpn(VpnConfiguration)` | `notifier.start()` → `setupVpn()` → `startForwarding()`. |
| `setupVpn(VpnConfiguration)` | Configures and establishes the tun: address `10.0.0.2/32`, session name, routes (default `0.0.0.0/0`), DNS (default `8.8.8.8`), blocking mode, MTU; returns `false` if `establish()` yields `null`. |
| `setAsUndernlyingNetwork()` | API ≥ 22 only: declares the VPN network itself as underlying so apps see the network as available. |
| `findVpnNetwork()` | Scans `ConnectivityManager.getAllNetworks()` for the network owning `10.0.0.2`. |
| `startForwarding()` | Creates the `Forwarder` with the tun fd and a `RelayTunnelListener` bound to the service handler; calls `forward()`. |
| `close()` | `notifier.stop()`, `forwarder.stop()`, `vpnInterface.close()`; idempotent. |
| `RelayTunnelConnectionStateHandler` | Static `Handler` (main looper): connected → `setFailure(false)`, disconnected → `setFailure(true)`; ignores messages when `!isRunning()`. |

### 2.3 `Notifier`

| Member | Contract |
|--------|----------|
| `NOTIFICATION_ID = 42` | Fixed id; updates reuse it. |
| `CHANNEL_ID = "Gnirehtet"` | O+ channel; created on start, deleted on stop. |
| `start()` | Clears the failure flag, creates the channel if needed, `startForeground(42, …)`. |
| `stop()` | `stopForeground(true)`; deletes the channel on O+. |
| `setFailure(boolean)` | Rebuilds/updates the notification only on state change. |
| `createNotification(boolean)` | Title app name; text/icon depend on failure; adds the Stop action. |
| `createStopAction()` | `PendingIntent.getService` (one-shot) for the stop intent, with `ic_close_24dp`. |

### 2.4 `Forwarder`

| Member | Contract |
|--------|----------|
| `EXECUTOR_SERVICE` (static, fixed pool of 3) | Both pump loops + wake-up task. |
| `BUFSIZE = 0x10000` | Read buffer for both directions. |
| `DUMMY_ADDRESS = 42.42.42.42`, `DUMMY_PORT = 4242` | Wake-up datagram target. |
| `Forwarder(VpnService, FileDescriptor, RelayTunnelListener)` | Creates the `PersistentRelayTunnel`. |
| `forward()` | Submits `forwardDeviceToTunnel` and `forwardTunnelToDevice`. |
| `stop()` | `tunnel.close()`, cancel both futures, `wakeUpReadWorkaround()`. |
| `forwardDeviceToTunnel(Tunnel)` | Blocking loop VPN fd → `tunnel.send()`; drops non-IPv4 packets. |
| `forwardTunnelToDevice(Tunnel)` | Blocking loop `tunnel.receive()` → `IPPacketOutputStream`. |
| `wakeUpReadWorkaround()` | Best-effort dummy UDP datagram on the executor (see [05](05-edge-cases.md)). |

### 2.5 `Tunnel`, `RelayTunnel`

| Member | Contract |
|--------|----------|
| `Tunnel.send(byte[], int)` | Blocking; sends one packet. |
| `Tunnel.receive(byte[])` | Blocking; returns bytes read, `-1` on EOF. |
| `Tunnel.close()` | Blocking; releases the transport. |
| `RelayTunnel.LOCAL_ABSTRACT_NAME = "gnirehtet"` | Abstract socket name created by `adb reverse`. |
| `RelayTunnel.open(VpnService)` | Factory; parameter unused since the local-socket switch. |
| `connect()` | Connects the `LocalSocket` then performs the client-id handshake. |
| `readClientId(InputStream)` | `DataInputStream.readInt()`; logs the id with `Binary.unsigned`. |
| `send` / `receive` | Raw writes/reads on the socket streams; optional verbose hex dumps. |
| `close()` | `shutdownInput`/`shutdownOutput` then `close`; wraps `IOException` in `RuntimeException`. |

### 2.6 `RelayTunnelProvider`

| Member | Contract |
|--------|----------|
| `DELAY_BETWEEN_ATTEMPTS_MS = 5000` | Backoff from the last recorded failure. |
| `getCurrentTunnelLock` | Serializes whole `getCurrentTunnel()` calls (one connector at a time). |
| `getCurrentTunnel()` | Fast path if a tunnel exists; otherwise backoff, create, connect, notify; returns the shared `RelayTunnel`. Throws `IOException`/`InterruptedException`. |
| `connectTunnel(boolean)` | Calls `connect()`, notifies connected, or records failure / conditionally notifies disconnect and rethrows. |
| `invalidateTunnel()` | If a tunnel exists: record failure, close, clear, notify disconnected. |
| `invalidateTunnel(Tunnel)` | Conditional variant guarding against invalidating a newer tunnel. |
| `touchFailure()` | Updates `lastFailureTimestamp` (drives the backoff). |
| `waitUntilNextAttemptSlot()` | Waits until `lastFailure + 5 s`; skips on first attempt; loops on recomputation. |
| `notifyConnected/Disconnected()` | Forward to the `RelayTunnelListener` (null-safe). |

### 2.7 `PersistentRelayTunnel`, `RelayTunnelListener`

| Member | Contract |
|--------|----------|
| `PersistentRelayTunnel(VpnService, RelayTunnelListener)` | Creates the provider. |
| `send(byte[], int)` | Retries until sent or stopped; invalidates the failed tunnel (identity-checked). |
| `receive(byte[])` | Retries on I/O error *and* on EOF; returns positive byte counts; throws when stopped. |
| `close()` | Sets `stopped`, invalidates the current tunnel unconditionally. |
| `RelayTunnelListener.MSG_RELAY_TUNNEL_CONNECTED/DISCONNECTED` | Empty-handler messages (`0`/`1`). |
| `notifyRelayTunnelConnected/Disconnected()` | Post to the wrapped `Handler` (main thread). |

### 2.8 `IPPacketOutputStream` and `Binary`

| Member | Contract |
|--------|----------|
| `MAX_IP_PACKET_LENGTH = 1 << 16` | One more than the largest IP packet; used as bound. |
| `buffer` (2× above) | Holds at most one partial packet between writes. |
| `write(byte[], int, int)` | Appends, sinks complete packets to the target, compacts leftovers; rejects `len > MAX`. |
| `write(int)` | Single-byte variant; fails if the buffer is full. |
| `sink()` / `sinkPacket()` | Extract and write complete packets; clears on non-IPv4; waits on incomplete. |
| `readPacketVersion(ByteBuffer)` | High nibble of the first readable byte, or `-1` if empty. |
| `readPacketLength(ByteBuffer)` | Unsigned 16-bit at offset +2, or `-1` if not yet available. |
| `Binary.unsigned(byte/short/int)` | Sign-free conversions. |
| `Binary.buildPacketString(byte[], int)` | ≤ 20-byte hex dump for verbose logs. |

### 2.9 Configuration classes

| Member | Contract |
|--------|----------|
| `VpnConfiguration()` | Empty DNS servers and routes (service applies defaults). |
| `VpnConfiguration(InetAddress[], CIDR[])` | Explicit config. |
| `getDnsServers()`, `getRoutes()` | Accessors. |
| Parcelable | Writes count + raw address bytes + typed `CIDR[]`. |
| `CIDR.parse(String)` | `a.b.c.d[/prefix]`; default prefix 32; invalid → `InvalidCIDRException`. |
| `CIDR.getAddress()`, `getPrefixLength()`, `toString()` | Accessors / `a.b.c.d/len`. |
| `Net.toInetAddress(es)`, `toCIDR(s)` | String/bytes ↔ objects; wraps checked errors in `IllegalArgumentException`. |
| `Net.getLocalhostIPv4()` | Unused vestige. |
| `InvalidCIDRException.getCIDR()` | The offending string. |

## 3. Android manifest

[`app/src/main/AndroidManifest.xml`](../../app/src/main/AndroidManifest.xml)

| Element | Setting | Why |
|---------|---------|-----|
| `uses-permission` | `ACCESS_NETWORK_STATE` | `ConnectivityManager.getAllNetworks()` / `getLinkProperties()` in `findVpnNetwork()`. |
| | `FOREGROUND_SERVICE` | Mandatory for foreground services on API 28+ (targetSdk 29). |
| | `INTERNET` | Used by the wake-up `DatagramSocket` and historically by the loopback tunnel. |
| `application` | `allowBackup="false"`, `icon="@null"` | No state to back up; no launcher icon. |
| | `label="@string/app_name"` | "Gnirehtet" (not translatable). |
| | `theme="@style/AppTheme"` | Base theme (empty customization). |
| activity | `exported="true"`, `permission=WRITE_SECURE_SETTINGS` | Reachable by adb shell only. |
| | `theme="@style/Theme.Transparent"` | Invisible, floating, no title. |
| | intent-filter | START and STOP actions. |
| service | `exported="true"`, `permission=BIND_VPN_SERVICE` | Required by the VpnService contract. |
| | intent-filter `android.net.VpnService` | Lets the system bind the service as a VPN. |

## 4. Resources

| Resource | Value / purpose |
|----------|-----------------|
| `values/strings.xml` | `app_name` = "Gnirehtet"; `relay_connected`, `relay_disconnected`, `stop_vpn`. |
| `values-fr/strings.xml` | French translations of the three notification strings. |
| `values/styles.xml` | `AppTheme` (base) and `Theme.Transparent` (activity). |
| `drawable/ic_usb_24dp.xml` | Notification icon while connected. |
| `drawable/ic_report_problem_24dp.xml` | Notification icon while disconnected. |
| `drawable/ic_close_24dp.xml` | Stop action icon. |

## 5. Build configuration

[`app/build.gradle`](../../app/build.gradle)

| Setting | Value |
|---------|-------|
| Plugin | `com.android.application` (AGP 3.5.0, root `build.gradle`) |
| `compileSdkVersion` | 28 |
| `buildToolsVersion` | 28.0.3 |
| `applicationId` | `com.genymobile.gnirehtet` |
| `minSdkVersion` | 21 (Android 5.0) |
| `targetSdkVersion` | 29 |
| `versionCode` / `versionName` | 9 / 2.5.1 |
| `archivesBaseName` | `gnirehtet` → APK `gnirehtet-release.apk` (copied as `gnirehtet.apk` by [`release/`](../../release)) |
| Release `minifyEnabled` | `false` (ProGuard rules file present but unused) |
| Dependencies | JUnit 4.12 (unit tests), Espresso 2.2.2 (no instrumented tests in tree) |
| Extra gradle scripts | checkstyle (`config/android-checkstyle.gradle`, `ignoreFailures = true`) and signing (`config/android-signing.gradle`, active when `RELEASE_STORE_FILE` is defined) |

Useful commands:

```bash
./gradlew :app:assembleDebug          # debug APK
./gradlew :app:assembleRelease        # release APK (signed if configured)
./gradlew :app:testDebugUnitTest      # unit tests (IPPacketOutputStream)
./gradlew :app:check                  # tests + checkstyle
./gradlew debugJava releaseJava       # root tasks: app + Java relay
./gradlew debugRust releaseRust       # root tasks: app + Rust relay
```

## 6. Unit tests

[`TestIPPacketOutputSteam`](../../app/src/test/java/com/genymobile/gnirehtet/TestIPPacketOutputSteam.java)
covers exactly the framing contract:

1. `testSimplePacket` — a 32-byte packet fed in 14+14+4 byte fragments is
   written to the target only once complete, byte-for-byte.
2. `testSeveralPacketsAtOnce` — 70 bytes containing two full packets (64 B)
   plus 6 bytes cause exactly **two** individual write calls; the remaining 26
   bytes complete the third packet and a third write call.

There are no Android instrumentation tests in the repository despite the
Espresso dependency.

## 7. Extension guide

| I want to… | Touch |
|------------|-------|
| Add a start option (e.g. a flag) | New `EXTRA_*` in `GnirehtetActivity`, extend `VpnConfiguration` (parcel it), consume it in `GnirehtetService.setupVpn()`. Pass it as `--es`/`--esa` from the CLI (`--esa` for arrays). |
| Change the reconnect delay | `RelayTunnelProvider.DELAY_BETWEEN_ATTEMPTS_MS` (also affects how long the notification stays in the failure state). |
| Change the VPN address/MTU | `GnirehtetService.VPN_ADDRESS` / `MTU`. The relay does not hardcode the client address (it swaps addresses), but `findVpnNetwork()` matches `VPN_ADDRESS`, so keep the two in sync. |
| Change the wake-up destination | `Forwarder.DUMMY_ADDRESS` / `DUMMY_PORT`; remember that the address must be captured by the configured routes. |
| Enable per-packet logging | Set `GnirehtetService.VERBOSE = true` and rebuild; watch `adb logcat -s RelayTunnel`. |
| Add a notification state | Add a message constant to `RelayTunnelListener`, post it from the data plane, handle it in `GnirehtetService.RelayTunnelConnectionStateHandler` and extend `Notifier`. |
| Change the handshake protocol | `RelayTunnel.readClientId()` plus **both** relay implementations (`relay-java/.../Client.java`, `relay-rust/src/relay/client.rs`) — it is a cross-implementation protocol invariant. |
| Transport something else than raw IPv4 | `Forwarder` (version filtering), `IPPacketOutputStream` (framing), and the relay parsers. Large change; the current design deliberately assumes IPv4. |

## 8. Document provenance

- Written against version **2.5.1**, commit `1eb2e58` (the current checkout).
- Cross-checked against the two relay implementations for protocol details
  (client id = big-endian 4-byte integer; address swapping in `Packetizer`).
- Historical rationale quotes come from the repository's git history
  (`f868180`, `0d832e0`, `cc367ff`, `301867e`, `47fae70`, `c4a9313`,
  `4ab392e`, `1050c7d`, `a1bb3c6`, `8eb8683`, `325c331`).
- When code and documentation disagree, trust the code, then fix this
  documentation.

[← Previous: edge cases](05-edge-cases.md) · [Index](README.md)
