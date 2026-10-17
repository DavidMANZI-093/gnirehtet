# 02 — Control plane: intents, VPN setup, notifications

[← Previous: architecture](01-architecture.md) · [Index](README.md) · [Next: data plane →](03-data-plane.md)

This chapter follows the control path: how the computer starts and stops the
client, how configuration reaches the VPN service, how the VPN is established,
and how the user is informed. The classes involved are `GnirehtetActivity`,
`GnirehtetService`, `Notifier`, `VpnConfiguration`, `CIDR`, `Net` and
`InvalidCIDRException`.

## 1. The intent protocol

The client is controlled exclusively through Android intents sent by adb:

```bash
# start (component explicitly named; extras optional)
adb shell am start -a com.genymobile.gnirehtet.START \
    -n com.genymobile.gnirehtet/.GnirehtetActivity \
    --esa dnsServers 8.8.8.8,8.8.4.4 \
    --esa routes 10.0.0.0/8,192.168.0.0/16

# stop
adb shell am start -a com.genymobile.gnirehtet.STOP \
    -n com.genymobile.gnirehtet/.GnirehtetActivity
```

| Intent element | Value |
|---|---|
| Action (start) | `com.genymobile.gnirehtet.START` (`GnirehtetActivity.ACTION_GNIREHTET_START`) |
| Action (stop) | `com.genymobile.gnirehtet.STOP` (`GnirehtetActivity.ACTION_GNIREHTET_STOP`) |
| Component | `com.genymobile.gnirehtet/.GnirehtetActivity` |
| Extra `dnsServers` | `String[]` of IP addresses; `--esa` makes `am` split the comma-separated value |
| Extra `routes` | `String[]` of CIDRs; same mechanism |

Things worth knowing:

- **`--esa` (extra string array)** is what the desktop CLI uses
  (`relay-rust/src/main.rs`, `relay-java/.../Main.java`): a single
  comma-separated argument becomes a `String[]`.
- Both extras are optional; missing extras become empty arrays, and the
  service then applies defaults (`0.0.0.0/0`, `8.8.8.8`).
- The component is always given explicitly (`-n`), because the activity has
  no `MAIN`/`LAUNCHER` intent filter — it is not reachable from a launcher.

### 1.1 Security: the activity permission

In [`AndroidManifest.xml`](../../app/src/main/AndroidManifest.xml):

```xml
<activity
    android:name="com.genymobile.gnirehtet.GnirehtetActivity"
    android:exported="true"
    android:permission="android.permission.WRITE_SECURE_SETTINGS"
    android:theme="@style/Theme.Transparent">
    <intent-filter>
        <action android:name="com.genymobile.gnirehtet.START" />
        <action android:name="com.genymobile.gnirehtet.STOP" />
    </intent-filter>
</activity>
```

`android:permission` on a component means *the caller must hold that
permission*. `WRITE_SECURE_SETTINGS` is a signature|privileged permission that
ordinary apps never get, but the `shell` user (and therefore `adb shell`) does.
Result: the activity is exported — as required for adb to reach it — but only
the shell/computer side can actually invoke it. Apps cannot silently enable or
disable reverse tethering.

### 1.2 Why an Activity and not a receiver or service?

Recent Android versions refuse to start services or broadcast receivers
directly from the shell (`Background execution limits`), and the VPN
authorization flow requires an `Activity` anyway (`startActivityForResult`).
`GnirehtetActivity` (commit `f868180`) replaced the older
`GnirehtetControlReceiver` and `AuthorizationActivity`. It is invisible: the
manifest gives it `@style/Theme.Transparent` (floating, no title, transparent
background), and `onCreate()` never calls `setContentView()`.

## 2. `GnirehtetActivity` walkthrough

[Source](../../app/src/main/java/com/genymobile/gnirehtet/GnirehtetActivity.java)

### 2.1 `onCreate()` → `handleIntent()`

```java
protected void onCreate(Bundle savedInstanceState) {
    super.onCreate(savedInstanceState);
    handleIntent(getIntent());
}
```

`handleIntent()` reads `intent.getAction()` and dispatches:

- `ACTION_GNIREHTET_START`: builds a `VpnConfiguration` from the extras and
  calls `startGnirehtet(config)`. The returned boolean says whether the
  activity may finish immediately.
- `ACTION_GNIREHTET_STOP`: calls `stopGnirehtet()`, then finishes.
- Any other action: finishes silently (still logs the request).

The activity is deliberately *not* single-use-per-process: if the VPN consent
dialog is up, `finish = false` and the activity stays alive to receive
`onActivityResult()`.

### 2.2 `createConfig()` — parsing the extras

```java
String[] dnsServers = intent.getStringArrayExtra(EXTRA_DNS_SERVERS); // may be null
String[] routes = intent.getStringArrayExtra(EXTRA_ROUTES);          // may be null
return new VpnConfiguration(Net.toInetAddresses(dnsServers), Net.toCIDRs(routes));
```

- `null` arrays are normalized to empty arrays *first*, so `Net.toInetAddresses()`
  never sees a null.
- `Net.toInetAddresses()` maps each string through `InetAddress.getByName()`
  (IPv4 or IPv6 textual forms; the rest of the pipeline is IPv4-only).
- `Net.toCIDRs()` maps each string through `CIDR.parse()`; a malformed value
  throws `IllegalArgumentException`/`InvalidCIDRException` inside the activity —
  i.e. a bad `--esa routes` value crashes the activity (the service is never
  started). In practice the CLI passes user input verbatim, so a typo in
  `-r`/routes is detected here.

### 2.3 `startGnirehtet()` — the consent dance

```java
Intent vpnIntent = VpnService.prepare(this);
if (vpnIntent == null) {
    // already authorized
    GnirehtetService.start(this, config);
    return true;                 // activity may finish now
}
requestAuthorization(vpnIntent, config);
return false;                    // keep the activity alive for the result
```

- `VpnService.prepare(Context)` returns `null` if this app already has VPN
  consent, otherwise an `Intent` that must be launched to ask the user.
- In the accepted case, `GnirehtetService.start()` sends the service the
  `ACTION_START_VPN` intent with the configuration as a `Parcelable` extra,
  using `startForegroundService()` on Android O+ (the service must then call
  `startForeground()` quickly — `Notifier.start()` does it immediately).
- Authorization is a one-time-per-install affair (until the user revokes it in
  system settings); afterwards every start takes the fast path.

### 2.4 `onActivityResult()` — after the system dialog

```java
if (requestCode == VPN_REQUEST_CODE && resultCode == RESULT_OK) {
    GnirehtetService.start(this, requestedConfig);
}
requestedConfig = null;
finish();
```

- `requestedConfig` is the field saved before launching the dialog.
- If the user declines, the result is not `RESULT_OK` and nothing is started.
- The activity always finishes at the end. Note that if the process had been
  killed while the dialog was displayed, `requestedConfig` would be `null`;
  `GnirehtetService.start()` tolerates this because the extras are parceled
  with a null value, and the service falls back to a default
  `VpnConfiguration` when the extra is missing.

### 2.5 Control flow summary

```
adb: am start -a …START
        │
        ▼
GnirehtetActivity.onCreate()
        │
        ▼
handleIntent()
        │ START
        ▼
createConfig() ──► VpnConfiguration(InetAddress[] dns, CIDR[] routes)
        │
        ▼
VpnService.prepare(this)
   ├── null (already authorized) ─► GnirehtetService.start() ─► finish()
   └── Intent (needs consent)
           │
           ▼
   startActivityForResult(system dialog, VPN_REQUEST_CODE)
           │
           ├── RESULT_OK  ─► GnirehtetService.start() ─► finish()
           └── canceled   ─► finish()
```

```
adb: am start -a …STOP  ─► GnirehtetActivity ─► GnirehtetService.stop() ─► finish()
```

## 3. Configuration classes

### 3.1 `VpnConfiguration`

[Source](../../app/src/main/java/com/genymobile/gnirehtet/VpnConfiguration.java)

An immutable value object with two fields:

| Field | Type | Meaning |
|-------|------|---------|
| `dnsServers` | `InetAddress[]` | DNS servers to configure on the VPN interface (empty ⇒ `8.8.8.8`) |
| `routes` | `CIDR[]` | Address ranges to capture (empty ⇒ `0.0.0.0/0`) |

It implements `Parcelable` because it travels from the activity to the service
inside an `Intent`:

- `writeToParcel()` writes the count of DNS servers, then each address as a raw
  `byte[]` (`InetAddress.getAddress()`), then the routes as a typed array
  (`dest.writeTypedArray(routes, 0)`).
- The private `Parcel` constructor rebuilds `InetAddress` objects with
  `InetAddress.getByAddress()`; failures become `AssertionError` (impossible
  for data that was previously an address).
- The no-argument constructor (`VpnConfiguration()`) produces empty arrays; the
  service uses it when the intent carries no configuration at all.

> **Why not pass strings?** Parceling raw bytes avoids re-resolving names and
> avoids ambiguous parsing twice. Note that `InetAddress.getByName()` in the
> activity is what would resolve a *hostname* if one were passed — the intent
> extras are free-form strings, not validated as literals.

### 3.2 `CIDR`

[Source](../../app/src/main/java/com/genymobile/gnirehtet/CIDR.java)

An address plus prefix length, e.g. `10.0.0.0/8`.

- `CIDR.parse(String)` splits on `/`:
  - with a slash: address = left part, prefix = `Integer.parseInt(right part)`;
  - without a slash: prefix defaults to **32** (a single host).
- Errors (`UnknownHostException` wrapped as `IllegalArgumentException`,
  `NumberFormatException`) are logged and rethrown as `InvalidCIDRException`
  (which keeps the offending string for `getCIDR()`).
- Also `Parcelable`, same style as `VpnConfiguration`.
- `toString()` yields `address/prefix`.

The prefix value itself is not range-checked here; `VpnService.Builder.addRoute()`
will reject an out-of-range prefix with an `IllegalArgumentException` when the
VPN is set up in the service.

### 3.3 `Net` and `InvalidCIDRException`

[`Net`](../../app/src/main/java/com/genymobile/gnirehtet/Net.java) is a static
helper: `toInetAddress(String)`, `toInetAddress(byte[])`,
`toInetAddresses(String...)`, `toCIDR(String)`, `toCIDRs(String...)`. It converts
checked exceptions into `IllegalArgumentException` so callers in the intent path
do not have to deal with them. `getLocalhostIPv4()` is a leftover from an older
transport (TCP to `127.0.0.1`) and is unused in 2.5.1.

[`InvalidCIDRException`](../../app/src/main/java/com/genymobile/gnirehtet/InvalidCIDRException.java)
is a plain checked exception carrying the invalid string; message is
`"Invalid CIDR:<value>"` (missing space included).

## 4. `GnirehtetService` walkthrough

[Source](../../app/src/main/java/com/genymobile/gnirehtet/GnirehtetService.java)

`GnirehtetService extends VpnService`. Its jobs: own the VPN interface, own the
`Forwarder`, own the `Notifier`, and keep all three consistent.

### 4.1 Static entry points

```java
public static void start(Context context, VpnConfiguration config) { … ACTION_START_VPN … }
public static void stop(Context context) { … ACTION_CLOSE_VPN … }
```

Both use `context.startForegroundService(intent)` on API ≥ 26 and
`context.startService(intent)` otherwise. The start intent carries the
`Parcelable` `EXTRA_VPN_CONFIGURATION`; the stop intent is built by
`createStopIntent()` (shared with the notification's Stop action).

### 4.2 `onStartCommand()` — the only command dispatcher

```java
String action = intent.getAction();
if (ACTION_START_VPN.equals(action)) {
    if (isRunning()) {
        Log.d(TAG, "VPN already running, ignore START request");
    } else {
        VpnConfiguration config = intent.getParcelableExtra(EXTRA_VPN_CONFIGURATION);
        if (config == null) config = new VpnConfiguration();
        startVpn(config);
    }
} else if (ACTION_CLOSE_VPN.equals(action)) {
    close();
}
return START_NOT_STICKY;
```

- **Idempotent start:** a START while `vpnInterface != null` is ignored, so
  double execution of `gnirehtet start` is harmless (existing session and its
  configuration are kept).
- **Default configuration:** a missing extra falls back to defaults rather than
  failing.
- **`START_NOT_STICKY`:** if Android kills the process, the service is *not*
  restarted automatically. After a kill the user must run the client again.
- `isRunning()` is simply `vpnInterface != null`, i.e. "the VPN interface was
  successfully established". It does **not** mean "the relay tunnel is
  connected"; those states are related but distinct (the notification reports
  the tunnel state).

### 4.3 `startVpn()` → `setupVpn()` — building the VPN interface

`startVpn()` performs three steps in order:

```java
notifier.start();                       // 1. mandatory foreground notification
if (setupVpn(config)) {                 // 2. create the tun interface
    startForwarding();                  // 3. start the data plane
}
```

`setupVpn()` uses the `VpnService.Builder`:

```java
Builder builder = new Builder();
builder.addAddress(VPN_ADDRESS, 32);          // 10.0.0.2/32
builder.setSession(getString(R.string.app_name)); // "Gnirehtet"

// routes: use the configured ones, or capture everything
if (routes.length == 0) builder.addRoute("0.0.0.0", 0);
else for (CIDR route : routes) builder.addRoute(route.getAddress(), route.getPrefixLength());

// DNS: use the configured ones, or Google Public DNS
if (dnsServers.length == 0) builder.addDnsServer("8.8.8.8");
else for (InetAddress dnsServer : dnsServers) builder.addDnsServer(dnsServer);

builder.setBlocking(true);                    // see below
builder.setMtu(MTU);                          // MTU = 0x4000
vpnInterface = builder.establish();           // null on failure or revoked consent
```

Rationales and details:

| Item | Why |
|------|-----|
| `10.0.0.2/32` | A single, private, fixed address for the tun interface. All device-originated packets get this source address, and relay replies target it. |
| `setSession("Gnirehtet")` | The session name shown by the system VPN UI (legacy Settings screen). |
| Default route `0.0.0.0/0` | Captures **all** IPv4 traffic. The `-r`/routes option of the CLI allows capturing only selected subnets instead. |
| Default DNS `8.8.8.8` | The VPN must advertise a resolver, otherwise DNS resolution on the device can break entirely. The actual queries are UDP packets that go through the relay like any other traffic; Google DNS works from the host's internet connection. |
| `setBlocking(true)` | By default `establish()` returns a **non-blocking** file descriptor, but `FileChannel` cannot be registered with a selector (see the code comment: "FileChannel is not selectable, that's stupid!"). Rather than busy-polling the fd, the client switches to blocking I/O and uses two plain threads. |
| `MTU = 0x4000` (16384) | Deliberate tuning. Bigger packets mean fewer packets and fewer syscalls for adb/gnirehtet when transferring data. The comment warns that higher values (`0x8000`, `0xFFFF`) *and* lower values (1500) have much worse performance (commit `4ab392e` has measurements). |

If `establish()` returns `null` (consent revoked, or not prepared), the code
logs "VPN starting failed, please retry" and returns `false` — no forwarding
threads start. (The notification started in step 1 is left in place; see
[05](05-edge-cases.md).)

### 4.4 `setAsUndernlyingNetwork()` — making the VPN look "connected"

[Note the typo in the method name: `Undernlying`.]

```java
if (Build.VERSION.SDK_INT >= 22) {
    Network vpnNetwork = findVpnNetwork();
    if (vpnNetwork != null) {
        // so that applications knows that network is available
        setUnderlyingNetworks(new Network[] {vpnNetwork});
    }
} else {
    Log.w(TAG, "Cannot set underlying network, API version " + Build.VERSION.SDK_INT + " < 22");
}
```

- `VpnService.setUnderlyingNetworks()` declares which real networks carry the
  VPN's traffic. Here there is no real underlying network on the device (the
  traffic goes out through the computer), so the code declares the **VPN
  network itself** as its underlying network.
- Effect: the system ConnectivityService then considers the VPN network as a
  valid, connected network, and apps are notified that a usable network exists.
  Without this, some apps would refuse to send traffic (or wait forever for a
  network callback) on API 22+.
- `findVpnNetwork()` scans `ConnectivityManager.getAllNetworks()` and returns
  the first network whose `LinkProperties.getLinkAddresses()` contains
  `10.0.0.2` — i.e. the interface just created. Requires the
  `ACCESS_NETWORK_STATE` permission.
- On API 21 the call is unavailable; the client logs a warning and proceeds
  (the VPN still works, but network availability notifications are less
  reliable on Lollipop).

### 4.5 `startForwarding()` and `close()`

```java
forwarder = new Forwarder(this, vpnInterface.getFileDescriptor(),
                          new RelayTunnelListener(handler));
forwarder.forward();
```

The service hands the raw `FileDescriptor` to the `Forwarder` and wires the
connection-state events to its own `Handler` (section 4.6). From this point on,
the data plane lives in [03](03-data-plane.md).

`close()` is the exact mirror image, in a deliberate order:

```java
if (!isRunning()) return;      // already closed

notifier.stop();               // remove foreground notification (+ channel)
forwarder.stop();              // close tunnel, cancel threads, unblock reads
vpnInterface.close();          // release the tun device
vpnInterface = null;
```

`forwarder.stop()` must happen before (or at least not after) closing the fd:
it closes the tunnel so the blocked relay read fails, cancels the two futures,
and pokes the VPN read with a dummy packet. Details in
[03](03-data-plane.md) and [05](05-edge-cases.md).

### 4.6 Connection-state handler

```java
private static final class RelayTunnelConnectionStateHandler extends Handler {
    public void handleMessage(Message message) {
        if (!vpnService.isRunning()) return; // ignore obsolete events
        switch (message.what) {
            case MSG_RELAY_TUNNEL_CONNECTED:    vpnService.notifier.setFailure(false); break;
            case MSG_RELAY_TUNNEL_DISCONNECTED: vpnService.notifier.setFailure(true);  break;
        }
    }
}
```

- The handler is instantiated as a field initializer, i.e. on the **main
  thread**, so it is bound to the main looper. Tunnel events produced by the
  forwarding threads are marshaled to the main thread by `RelayTunnelListener`
  (see [03](03-data-plane.md)).
- `isRunning()` filters events that arrive after a STOP: without this check, a
  late "disconnected" event could resurrect a notification after the user
  stopped the VPN.
- The handler only touches the `Notifier`; it deliberately does not close or
  restart anything (the `PersistentRelayTunnel` does that on its own threads).

## 5. `Notifier` — the foreground notification

[Source](../../app/src/main/java/com/genymobile/gnirehtet/Notifier.java)

Android O and later require a *foreground service* for this kind of ongoing
background work, and a foreground service must display a notification
immediately. `Notifier` encapsulates it.

| Member | Value / behavior |
|--------|------------------|
| `NOTIFICATION_ID` | `42` (fixed, so updates replace the same notification) |
| `CHANNEL_ID` | `"Gnirehtet"` (Android O+, created in `start()`, deleted in `stop()`) |
| Content title | app name, "Gnirehtet" |
| Content text | `relay_connected` ("Reverse tethering enabled") / `relay_disconnected` ("Disconnected from the relay server") |
| Small icon | `ic_usb_24dp` / `ic_report_problem_24dp` (white-only vector icons) |
| Action | `ic_close_24dp` + "Stop Gnirehtet" → `PendingIntent.getService()` with the same stop intent as `GnirehtetService.createStopIntent()` (one-shot) |

State machine:

- `start()`: resets `failure = false`, creates the channel on O+, then
  `context.startForeground(42, notification(false))`. Calling this first thing
  in `startVpn()` is what protects the service from Android's
  "startForegroundService must call startForeground" timeout.
- `setFailure(boolean)`: only acts when the value *changes*; rebuilds the
  notification with the appropriate text/icon and updates it via
  `NotificationManager.notify(42, …)`. Called on the main thread from the
  connection-state handler.
- `stop()`: `stopForeground(true)` (removes the notification and cancels the
  service's foreground state) and deletes the channel on O+.

The notification is the only UI of the whole app. Its *Stop* action goes
through the same public path as `adb shell am start …STOP` (a service intent),
so there is exactly one shutdown code path.

### 5.1 Notification state vs tunnel state

| Moment | Notification |
|--------|--------------|
| `startVpn()` before the first relay connection succeeds | "Reverse tethering enabled" (optimistic; `failure=false`) |
| First connection attempt fails (and every later disconnection) | "Disconnected from the relay server" + warning icon |
| Reconnection succeeds | "Reverse tethering enabled" + USB icon |
| VPN running, relay connected, but host has no internet | still "Reverse tethering enabled" — the client cannot detect that |

The optimistic initial state is intentional: the client has no way to know
whether the relay is reachable until it tries, and the first attempt result
arrives typically within milliseconds.

## 6. Complete control sequences

### 6.1 First start (consent required)

```
CLI                adb                    GnirehtetActivity            GnirehtetService
 │  am start …START  │                            │                          │
 ├──────────────────►│  deliver intent            │                          │
 │                   ├───────────────────────────►│ onCreate/handleIntent    │
 │                   │                            │ VpnService.prepare() ─► non-null
 │                   │                            │ startActivityForResult(dialog)
 │                   │◄─── user accepts ──────────┤ onActivityResult OK      │
 │                   │                            ├─ start() ──────────────►│ onStartCommand(START_VPN)
 │                   │                            │                          │ notifier.start()
 │                   │                            │                          │ setupVpn(): establish() → fd
 │                   │                            │                          │ setUnderlyingNetworks()
 │                   │                            │                          │ Forwarder.forward() → 2 threads
 │                   │                            │ finish()                 │
```

### 6.2 Later starts, stops, and restarts

- **Start again while running:** the activity finishes immediately; the service
  logs "VPN already running, ignore START request" and keeps the old
  configuration (extras of the second START are discarded).
- **Stop:** activity → `GnirehtetService.stop()` → `onStartCommand(CLOSE_VPN)`
  → `close()` (notification removed, forwarding stopped, fd closed). The
  service itself keeps existing as a started (non-foreground) service; see
  [05](05-edge-cases.md).
- **Restart:** `gnirehtet restart` = stop + start; there is a small window
  between the two intents where the VPN is torn down. A fresh session creates a
  new tun and a new tunnel.

[← Previous: architecture](01-architecture.md) · [Index](README.md) · [Next: data plane →](03-data-plane.md)
