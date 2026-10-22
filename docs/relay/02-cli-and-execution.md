# 02 — CLI, ADB, and the control plane

[← Previous: architecture](01-architecture.md) · [Index](README.md) · [Next: tunnel and client →](03-tunnel-and-client.md)

This chapter covers everything that happens before and around the relay's packet
loop: argument parsing, logging, `adb` subprocesses, APK discovery, install and
version checks, the `run`/`autorun`/`autostart` commands, Ctrl+C behavior, and
the ADB device monitor.

The data plane is documented starting in [03](03-tunnel-and-client.md). The
control plane is deliberately boring; it exists so the data plane can assume a
started client and a working reverse tunnel.

## 1. Command-line shape

The CLI is defined with `clap`'s derive API in `relay-rust/src/cli.rs`. The
binary is named `gnirehtet` and reports version `2.5.1`.

```text
gnirehtet [-v|--verbose] <COMMAND> [ARGS]
```

`-v/--verbose` is a **global** option. It may appear before or after the
subcommand:

```text
gnirehtet -v run
gnirehtet run --verbose
```

There is no `-vv`, no log-level option, and no CLI way to select `Trace`.
Verbose means "threshold = Debug"; otherwise the logger keeps its static `Info`
threshold. `Trace` exists in the logger and is used by packet-dump paths, but it
is only reachable if library code calls `logger::set_threshold(LevelFilter::Trace)`.

### 1.1 Commands

| Command | Device serial? | DNS/routes/port? | Purpose |
|---------|----------------|------------------|---------|
| `install [SERIAL]` | optional | no | Install the APK with `adb install -r`. |
| `uninstall [SERIAL]` | optional | no | Uninstall the package. |
| `reinstall [SERIAL]` | optional | no | Uninstall, then install. |
| `run [SERIAL]` | optional | yes | Install/start the client and run the relay; Ctrl+C stops the client. |
| `autorun` | no | yes | Monitor devices, start clients automatically, and run the relay. |
| `start [SERIAL]` | optional | yes | Install/start a client and create the reverse tunnel, then exit. |
| `autostart` | no | yes | Monitor devices and start clients; does not run the relay. |
| `stop [SERIAL]` | optional | no | Send the STOP intent. |
| `restart [SERIAL]` | optional | yes | Stop, then start. |
| `tunnel [SERIAL]` | optional | port only | Run `adb reverse localabstract:gnirehtet tcp:<port>`. |
| `relay` | no | port only | Run only the relay server in the current terminal. |

`SERIAL` is an optional positional argument. If more than one device is
connected, the user must pass it; otherwise `adb` chooses or errors according to
its own rules. The relay does not inspect `adb devices` before ordinary commands.

### 1.2 Shared options

The `run`, `autorun`, `start`, `autostart`, and `restart` commands accept:

| Option | Default | Validation |
|--------|---------|------------|
| `-d, --dns-servers <DNS[,DNS2,...]>` | `8.8.8.8` | Each comma-separated entry must parse as an IPv4 address; empty entries are rejected. |
| `-r, --routes <ROUTE[,ROUTE2,...]>` | `0.0.0.0/0` | Each entry must be an IPv4 address, optionally with `/prefix` where prefix ≤ 32. |
| `-p, --port <PORT>` | `31416` | Parsed as `u16`; zero is rejected. |

The parser trims entries and rejoins them with commas. It does **not** normalize
networks (for example, it does not turn `10.0.0.1/8` into `10.0.0.0/8`) and does
not support IPv6 or hostnames. Validation catches syntax, not routing semantics.

`tunnel` and `relay` accept only `-p/--port`; `install`, `uninstall`, `reinstall`,
and `stop` accept only the optional serial.

### 1.3 Defaults in code

| Constant | Value |
|----------|-------|
| `DEFAULT_PORT` | `31416` |
| `DEFAULT_DNS_SERVERS` | `8.8.8.8` |
| `DEFAULT_ROUTES` | `0.0.0.0/0` |

The CLI test suite checks that verbosity defaults to false and that `-v`/`--verbose`
works both before and after the subcommand.

## 2. Main and logging

`main.rs` is intentionally small:

```rust
fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.verbose {
        logger::set_threshold(LevelFilter::Debug);
    }
    match cli.cmd.execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(target: "Main", "Execution error: {}", err);
            ExitCode::from(3)
        }
    }
}
```

Consequences:

- `clap` handles `--help`, `--version`, and argument errors; those exit before
  `main`'s match.
- All `ExecutionError` values are logged at `ERROR` and produce exit code `3`.
- There is no special exit code for a bind failure, adb failure, or Ctrl+C. The
  `run` Ctrl+C handler explicitly exits with `0`.

### 2.1 Logger

`logger.rs` implements a tiny global logger rather than pulling in `log` or
`env_logger`:

```text
static AtomicLevelFilter THRESHOLD = Info

set_threshold(level)   // relaxed atomic store
is_enabled(level)      // level <= THRESHOLD
emit(level, target, args)
```

The level order is:

```text
Off < Error < Warn < Info < Debug < Trace
```

`Off` disables everything. `Error` messages go to `stderr`; every other enabled
level goes to `stdout`. The format is:

```text
YYYY-MM-DD HH:MM:SS.mmm LEVEL Target: message
```

Timestamps use the local timezone via `chrono::Local`. Logging is synchronous
and does not lock beyond the atomic load; concurrent calls may interleave only at
the line level in the underlying stdio implementation. The atomic is
`Ordering::Relaxed`, which is sufficient for a threshold that is set once at
startup.

Macros: `error!`, `warn!`, `info!`, `debug!`, `trace!`, all requiring an
explicit `target:` argument. Connection modules use `cx_*!` wrappers that prefix
the flow's `ConnectionId` to every message.

### 2.2 What `-v` shows

At `Info` (default), the user sees command progress and connection lifecycle
messages. At `Debug` (`-v`), the relay adds event-loop details, state transitions,
interest changes, packet sizes, ACK behavior, and connection ids. `Trace` adds
raw packet hex dumps (first 20 bytes, grouped by 4) but is not reachable from the
CLI in this revision.

For a typical debugging session:

```bash
make relay-release
./relay-rust/target/release/gnirehtet -v run
```

or, if the relay is managed separately:

```bash
./relay-rust/target/release/gnirehtet -v relay
```

## 3. The executor

`executor.rs` implements `Command::execute`, which dispatches each command to a
small function. The interesting part is how commands compose.

### 3.1 `start`

`start` is the normal "make one device work" operation:

```text
start(serial, dns, routes, port)
  ├─ must_install_client(serial)?
  │    ├─ true  → install(serial)?; sleep 500 ms
  │    └─ false → continue
  ├─ log "Starting client..."
  ├─ tunnel(serial, port)?
  └─ adb shell am start -a d3fault.gnirehtet.START
        -n d3fault.gnirehtet/.GnirehtetActivity
        --esa dnsServers <csv>
        --esa routes <csv>
```

The 500 ms sleep after installation gives Android time to register the intent
actions. It is a heuristic, not a guarantee; `am start` will fail if the package
is not yet visible.

The reverse tunnel is created **before** the START intent. Consequently, when the
client starts, the host side is already mapped. The relay process, however, may
not be running yet; that is why `run` starts the client asynchronously and why
the Android client must tolerate connection failures and retry.

### 3.2 `run`

```text
run(serial, dns, routes, port)
  ├─ async_start(...)                     // spawn thread
  ├─ ctrlc::set_handler(|| {
  │      stop(serial);
  │      process::exit(0);
  │  })
  └─ relay(port)                          // blocks
```

`async_start` spawns a thread that calls `start`. The relay then runs on the main
thread. On Ctrl+C:

1. Log `Interrupted`.
2. Send the STOP intent via `adb` (same serial, if provided).
3. Exit the process with code 0, whether or not the STOP succeeded. A failure to
   stop is logged but does not change the exit code.

The `ctrlc` dependency is built with only the `termination` feature. Handler
installation itself can fail; that becomes `ExecutionError::Ctrlc` and exits
with code 3.

### 3.3 `autorun`

```text
autorun(dns, routes, port)
  ├─ spawn thread: autostart(dns, routes, port)
  └─ relay(port)
```

`autostart` constructs an `AdbMonitor` whose callback runs `async_start` for each
newly connected serial. The monitor thread keeps running. The relay runs on the
main thread.

Important distinction: `autorun` has **no Ctrl+C handler**. Pressing Ctrl+C
terminates the relay process directly. The Android clients that were started are
not sent a STOP intent; they keep running until stopped separately. This is a
deliberate difference from `run`.

### 3.4 `autostart`

`autostart` runs the monitor only:

```text
autostart(dns, routes, port)
  └─ AdbMonitor::monitor()   // loops forever
```

It does not bind the relay port. Use it when `gnirehtet relay` (or another
relay) is already running. Since the monitor loop has no normal termination, the
command ends only via a signal or an unhandled failure.

### 3.5 `relay`

`relay` logs the port and calls `crate::relay::run(port)`. It does not touch the
device and does not install a Ctrl+C handler. Its loop is infinite unless an I/O
error escapes.

### 3.6 `stop` and `restart`

`stop` sends:

```text
adb shell am start -a d3fault.gnirehtet.STOP \
  -n d3fault.gnirehtet/.GnirehtetActivity
```

`restart` calls `stop` and then `start`. It does not wait for the STOP to fully
take effect before starting again; the Android activity/service is expected to
serialize those operations.

### 3.7 `tunnel`

```text
adb reverse localabstract:gnirehtet tcp:<port>
```

This maps the device's abstract Unix socket to the host's loopback TCP port. The
command is idempotent from adb's perspective; repeating it replaces the mapping.
The relay itself never calls `adb`; only the control plane does.

### 3.8 `install`, `uninstall`, `reinstall`

These map directly to:

```text
adb install -r <apk>
adb uninstall d3fault.gnirehtet
```

`reinstall` is uninstall followed by install. There is no rollback if the install
fails after a successful uninstall. The APK path rules are described below.

## 4. APK discovery

`get_apk_path()` is deliberately minimal:

```rust
if let Some(path) = env::var_os("GNIREHTET_APK") {
    path.into_string().expect("invalid GNIREHTET_APK value")
} else {
    "gnirehtet.apk".to_string()
}
```

Therefore:

1. If `GNIREHTET_APK` is set, its value is used as-is.
2. Otherwise the relative path `gnirehtet.apk` is used, resolved against the
   process's current working directory.

There is **no** search of the executable's directory and no fallback path in this
revision. To run from a different directory, either `cd` to the directory
containing the APK, pass an absolute path through `GNIREHTET_APK`, or create a
symlink named `gnirehtet.apk` in the working directory.

`into_string()` panics if the environment variable is not valid Unicode; this is
treated as a startup configuration error rather than a recoverable one.

## 5. Client version gating

Before starting a client, `must_install_client` inspects the installed package:

```text
adb shell dumpsys package d3fault.gnirehtet
```

It looks for the literal substring `"    versionCode="` in stdout. If found, it
extracts the next whitespace-delimited token and compares it with the required
version code `"9"`:

```text
installed version code != "9"  → install
versionCode not found         → install
dumpsys command failed        → ExecutionError (no install attempt)
```

The check is intentionally string-based to avoid pulling in a regex dependency
(the source notes that a regex crate would increase binary size). It does not
compare `versionName`; it only checks the integer version code.

Constants in `executor.rs`:

| Constant | Value | Meaning |
|----------|-------|---------|
| `PACKAGE` | `d3fault.gnirehtet` | Android application id |
| `ACTIVITY` | `d3fault.gnirehtet/.GnirehtetActivity` | Invisible control activity |
| `ACTION_START` | `d3fault.gnirehtet.START` | Intent action to start the VPN |
| `ACTION_STOP` | `d3fault.gnirehtet.STOP` | Intent action to stop the VPN |
| `REQUIRED_APK_VERSION_CODE` | `"9"` | Expected `versionCode` |

If the package is not installed, `dumpsys package` usually returns success with
no matching `versionCode` line, so the relay proceeds to install. If adb itself
fails (device offline, server not running, permission issue), the output status is
non-success and the command returns an error rather than silently installing.

## 6. ADB invocation and errors

### 6.1 ADB path

`adb_monitor.rs::get_adb_path()` returns:

1. `$ADB` if set;
2. otherwise `"adb"` (resolved through `PATH`).

The same helper is used by the monitor, the installer, and all tunnel/intent
commands.

### 6.2 Building arguments

`create_adb_args(serial, args)` puts `-s <serial>` first when a serial is
provided, followed by the command arguments. This is equivalent to:

```text
adb -s R58N... install -r gnirehtet.apk
```

### 6.3 Running a command

`exec_adb` uses `std::process::Command::status()`, which inherits stdout/stderr,
so adb's own messages appear directly in the terminal. It records the command for
debug logging:

```text
DEBUG Executor: Execute: "adb" ["-s", "...", "reverse", ...]
```

Two error types capture the distinction between "could not start the process" and
"process ran but failed":

- `ProcessIoError` — `Command::new(...).status()` returned an `io::Error`
  (for example, `adb` not found). Displayed as
  `Command <cmd> failed: <io error>`.
- `ProcessStatusError` — the process started but exited non-zero or was killed by
  a signal. On Unix it distinguishes an exit value from signal termination.
  Displayed as `Command <cmd> returned with value <n>` or
  `Command <cmd> terminated by signal <n>`.

Both are wrapped by `ExecutionError`. `ExecutionError` also has `Io` (used when
the relay's `io::Error` is converted with `?`) and `Ctrlc` (handler setup).

`ProcessCommand` stores the command name and argument vector so error messages
are actionable without reconstructing the invocation.

### 6.4 Output capture for version checks

`must_install_client` uses `Command::output()` rather than `status()` because it
needs stdout. It still checks `output.status.success()` first. Stderr is not
parsed; if the command fails, the exit status is reported. This is a useful
distinction when debugging: a version check failure is a process-status error,
not an "install anyway" decision.

## 7. The ADB device monitor

`AdbMonitor` implements a minimal client for adb's `host:track-devices` service.
It is used only by `autostart` and therefore by `autorun`.

### 7.1 Connection and subscription

```text
connect to 127.0.0.1:5037
write "0012host:track-devices"
read 4 bytes "OKAY"
loop:
    read 4 hex digits (payload length)
    read exactly that many bytes
    parse the device list
```

The request is a 4-hex-digit length (`0012` = 18) followed by the service name
`host:track-devices`. This is adb's host protocol, not the device shell protocol.
The monitor uses a blocking `std::net::TcpStream`, not the relay's `mio` loop.

`ByteBuffer` (1024 bytes) holds a prefix of the stream. `consume_okay` waits for
four bytes and compares them with `OKAY`; then `next_packet` parses the length,
waits for the complete payload, consumes it, and returns a UTF-8 string. Lengths
larger than 1024 are rejected as invalid input; in practice a 1024-byte payload
cannot fit together with the 4-byte length prefix, so the maximum usable payload
is smaller than the nominal buffer size.

### 7.2 Parsing device state

Each payload is newline-separated device records, typically:

```text
0123456789ABCDEF\tdevice
FEDCBA9876543210\tunauthorized
```

`parse_connected_devices` keeps only entries whose second whitespace-delimited
field is exactly `device`. It ignores `offline`, `unauthorized`, and any other
state. It compares the new list with the previous list and invokes the callback
only for serials that were not present before:

```text
previous: [A]
current:  [A, B]  → callback(B)
previous: [A, B]
current:  [B]     → no callback
previous: [B]
current:  [A, B]  → callback(A)
```

This means a disconnect followed by a reconnect of the same device triggers a new
start attempt. The monitor does not invoke a stop callback on disconnect and does
not remove clients.

### 7.3 Reconnection and adb daemon repair

`monitor()` loops forever:

```rust
loop {
    if let Err(err) = self.track_devices() {
        error!("Failed to monitor adb devices: {}", err);
        repair_adb_daemon();
    }
}
```

`repair_adb_daemon` runs `adb start-server` and waits:

- 1 second if the daemon started successfully;
- 5 seconds if `adb start-server` failed or could not be spawned.

The delays are constants in `adb_monitor.rs`. There is no exponential backoff and
no maximum retry count. If `track-devices` closes cleanly (EOF), that is treated
as an error by `fill_buffer_from` / `consume_okay`, so the repair path is taken.

A monitor callback is synchronous on the monitor thread. `autostart`'s callback
does not run `adb` inline; it calls `async_start`, which spawns a new thread. This
keeps the monitor responsive while an install/start operation is in progress.

### 7.4 What the monitor does not do

- It does not run `adb devices` as a subprocess.
- It does not validate device authorization beyond filtering the state string.
- It does not stop the Android client when the device disappears.
- It does not coordinate multiple starts for the same serial; a flapping device
  can cause overlapping start threads.
- It does not recover relay connections; reconnect handling is on the Android
  client side.

## 8. Control-plane sequence diagrams

### 8.1 `gnirehtet run`

```text
user                gnirehtet main              adb                 device
 │                       │                       │                    │
 │  run [serial]         │                       │                    │
 ├──────────────────────►│                       │                    │
 │                       ├─ spawn start thread ──┼───────────────────►│
 │                       ├─ install Ctrl+C       │                    │
 │                       └─ relay(port)          │                    │
 │                                               │                    │
 │                       start thread:           │                    │
 │                       ├─ dumpsys package ────►│                    │
 │                       ├─ install if needed ───►│                    │
 │                       ├─ reverse tunnel ──────►│                    │
 │                       └─ am start START ──────►│──► GnirehtetActivity
 │                                                                    │
 │                       relay accepts ◄──────── adb reverse ─────────┤
 │                                                                    │
 │  Ctrl+C               │                       │                    │
 ├──────────────────────►│─ stop intent ────────►│──► STOP            │
 │                       └─ exit(0)              │                    │
```

### 8.2 `gnirehtet autorun`

```text
main                          monitor thread                 adb server
 │                                │                             │
 ├─ spawn autostart ─────────────►│                             │
 │                                ├─ host:track-devices ───────►│
 │                                │◄──── OKAY + device list ────┤
 │                                ├─ new serial → async_start ──┼──► start
 │                                │                             │
 └─ relay(port)                   │                             │
    (main blocks)                 └─ loop forever               │
```

## 9. Operational notes

- **Run from the APK directory** unless `GNIREHTET_APK` is set. A common failure
  is `adb: failed to stat gnirehtet.apk: No such file or directory`.
- **`run` is the only command that stops the Android client on Ctrl+C.** Use
  `gnirehtet stop [SERIAL]` after `autorun`, `relay`, or a killed process if the
  client is still active.
- **A relay bind failure is reported as an execution error and exits with code
  3.** Check whether another relay is already using the port.
- **Serial selection is delegated to adb.** If multiple devices are connected
  and no serial is supplied, adb will fail with its own message; the relay
  reports the non-zero status.
- **Verbose mode is diagnostic, not a different protocol.** It only changes the
  log threshold; packet handling is identical.

The next chapter moves from commands to the accepted tunnel connection and the
event loop that drives it: [03 — Tunnel server, selector, and client](03-tunnel-and-client.md).
