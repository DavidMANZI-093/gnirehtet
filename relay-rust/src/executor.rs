/*
 * Copyright (C) 2017 Genymobile
 * Copyright (C) 2026 David MANZI
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::{
    error, fmt, io,
    process::{self, ExitStatus},
    thread,
    time::Duration,
};

use crate::{
    adb_monitor::{AdbMonitor, get_adb_path},
    cli::Command,
    debug, error, info,
};

const TAG: &str = "Executor";

const PACKAGE: &str = "d3fault.gnirehtet";
const ACTIVITY: &str = "d3fault.gnirehtet/.GnirehtetActivity";
const ACTION_START: &str = "d3fault.gnirehtet.START";
const ACTION_STOP: &str = "d3fault.gnirehtet.STOP";
const REQUIRED_APK_VERSION_CODE: &str = "9";

impl Command {
    pub fn execute(self) -> Result<(), ExecutionError> {
        match self {
            Command::Install { serial } => install(serial.as_deref()),
            Command::Uninstall { serial } => uninstall(serial.as_deref()),
            Command::Reinstall { serial } => reinstall(serial.as_deref()),
            Command::Run {
                serial,
                dns_servers,
                routes,
                port,
            } => run(serial.as_deref(), &dns_servers, &routes, port),
            Command::Autorun {
                dns_servers,
                routes,
                port,
            } => autorun(&dns_servers, &routes, port),
            Command::Start {
                serial,
                dns_servers,
                routes,
                port,
            } => start(serial.as_deref(), &dns_servers, &routes, port),
            Command::Autostart {
                dns_servers,
                routes,
                port,
            } => autostart(&dns_servers, &routes, port),
            Command::Stop { serial } => stop(serial.as_deref()),
            Command::Restart {
                serial,
                dns_servers,
                routes,
                port,
            } => restart(serial.as_deref(), &dns_servers, &routes, port),
            Command::Tunnel { serial, port } => tunnel(serial.as_deref(), port),
            Command::Relay { port } => relay(port),
        }
    }
}

// ---------- commands ----------

fn install(serial: Option<&str>) -> Result<(), ExecutionError> {
    info!(target: TAG, "Installing gnirehtet client...");
    exec_adb(serial, vec!["install".into(), "-r".into(), get_apk_path()])
}

fn uninstall(serial: Option<&str>) -> Result<(), ExecutionError> {
    info!(target: TAG, "Uninstalling gnirehtet client...");
    exec_adb(serial, vec!["uninstall", PACKAGE])
}

fn reinstall(serial: Option<&str>) -> Result<(), ExecutionError> {
    uninstall(serial)?;
    install(serial)
}

fn start(
    serial: Option<&str>,
    dns_servers: &str,
    routes: &str,
    port: u16,
) -> Result<(), ExecutionError> {
    if must_install_client(serial)? {
        install(serial)?;
        // Wait a bit after installation so that intent actions are registered.
        thread::sleep(Duration::from_millis(500));
    }

    info!(target: TAG, "Starting client...");
    tunnel(serial, port)?;

    let adb_args = vec![
        "shell",
        "am",
        "start",
        "-a",
        ACTION_START,
        "-n",
        ACTIVITY,
        "--esa",
        "dnsServers",
        dns_servers,
        "--esa",
        "routes",
        routes,
    ];
    exec_adb(serial, adb_args)
}

fn stop(serial: Option<&str>) -> Result<(), ExecutionError> {
    info!(target: TAG, "Stopping client...");
    exec_adb(
        serial,
        vec!["shell", "am", "start", "-a", ACTION_STOP, "-n", ACTIVITY],
    )
}

fn restart(
    serial: Option<&str>,
    dns_servers: &str,
    routes: &str,
    port: u16,
) -> Result<(), ExecutionError> {
    stop(serial)?;
    start(serial, dns_servers, routes, port)
}

fn run(
    serial: Option<&str>,
    dns_servers: &str,
    routes: &str,
    port: u16,
) -> Result<(), ExecutionError> {
    // Start in parallel so that the relay server is ready when the client connects.
    async_start(serial, dns_servers, routes, port);

    let ctrlc_serial = serial.map(str::to_owned);
    ctrlc::set_handler(move || {
        info!(target: TAG, "Interrupted");
        if let Err(err) = stop(ctrlc_serial.as_deref()) {
            error!(target: TAG, "Cannot stop client: {}", err);
        }
        process::exit(0);
    })?;

    relay(port)
}

fn relay(port: u16) -> Result<(), ExecutionError> {
    info!(target: TAG, "Starting relay server on port {}...", port);
    crate::relay::run(port)?;
    Ok(())
}

fn autorun(dns_servers: &str, routes: &str, port: u16) -> Result<(), ExecutionError> {
    let dns_servers = dns_servers.to_owned();
    let routes = routes.to_owned();
    thread::spawn(move || {
        if let Err(err) = autostart(&dns_servers, &routes, port) {
            error!(target: TAG, "Cannot auto start clients: {}", err);
        }
    });
    relay(port)
}

fn autostart(dns_servers: &str, routes: &str, port: u16) -> Result<(), ExecutionError> {
    let dns_servers = dns_servers.to_owned();
    let routes = routes.to_owned();
    let mut monitor = AdbMonitor::new(Box::new(move |serial: &str| {
        async_start(Some(serial), &dns_servers, &routes, port);
    }));
    monitor.monitor();
    Ok(())
}

fn async_start(serial: Option<&str>, dns_servers: &str, routes: &str, port: u16) {
    let serial = serial.map(str::to_owned);
    let dns_servers = dns_servers.to_owned();
    let routes = routes.to_owned();
    thread::spawn(move || {
        if let Err(err) = start(serial.as_deref(), &dns_servers, &routes, port) {
            error!(target: TAG, "Cannot start client: {}", err);
        }
    });
}

fn tunnel(serial: Option<&str>, port: u16) -> Result<(), ExecutionError> {
    let port_arg = format!("tcp:{port}");
    exec_adb(
        serial,
        vec![
            "reverse".to_string(),
            "localabstract:gnirehtet".to_string(),
            port_arg,
        ],
    )
}

// ---------- execution helpers ----------

fn exec_adb<S: Into<String>>(serial: Option<&str>, args: Vec<S>) -> Result<(), ExecutionError> {
    let adb_args = create_adb_args(serial, args);
    let adb = get_adb_path();
    debug!(target: TAG, "Execute: {:?} {:?}", adb, adb_args);
    match process::Command::new(&adb).args(&adb_args[..]).status() {
        Ok(exit_status) => {
            if exit_status.success() {
                Ok(())
            } else {
                let cmd = ProcessCommand::new(adb, adb_args);
                Err(ProcessStatusError::new(cmd, exit_status).into())
            }
        }
        Err(err) => {
            let cmd = ProcessCommand::new(adb, adb_args);
            Err(ProcessIoError::new(cmd, err).into())
        }
    }
}

fn create_adb_args<S: Into<String>>(serial: Option<&str>, args: Vec<S>) -> Vec<String> {
    let mut command = Vec::<String>::new();
    if let Some(serial) = serial {
        command.push("-s".into());
        command.push(serial.to_string());
    }
    for arg in args {
        command.push(arg.into());
    }
    command
}

fn must_install_client(serial: Option<&str>) -> Result<bool, ExecutionError> {
    info!(target: TAG, "Checking gnirehtet client...");
    let args = create_adb_args(serial, vec!["shell", "dumpsys", "package", PACKAGE]);
    let adb = get_adb_path();
    debug!(target: TAG, "Execute: {:?} {:?}", adb, args);
    match process::Command::new(&adb).args(&args[..]).output() {
        Ok(output) => {
            if output.status.success() {
                // The "regex" crate makes the binary far bigger, so just parse the
                // versionCode manually.
                let dumpsys = String::from_utf8_lossy(&output.stdout[..]);
                if let Some(index) = dumpsys.find("    versionCode=") {
                    let start = index + "    versionCode=".len();
                    let installed_version_code = dumpsys[start..].split_whitespace().next();
                    Ok(installed_version_code != Some(REQUIRED_APK_VERSION_CODE))
                } else {
                    // versionCode not found
                    Ok(true)
                }
            } else {
                let cmd = ProcessCommand::new(adb, args);
                Err(ProcessStatusError::new(cmd, output.status).into())
            }
        }
        Err(err) => {
            let cmd = ProcessCommand::new(adb, args);
            Err(ProcessIoError::new(cmd, err).into())
        }
    }
}

#[inline]
fn get_apk_path() -> String {
    if let Some(env_adb) = std::env::var_os("GNIREHTET_APK") {
        env_adb.into_string().expect("invalid GNIREHTET_APK value")
    } else {
        "gnirehtet.apk".to_string()
    }
}

// ---------- Errors ----------

#[derive(Debug)]
pub enum ExecutionError {
    ProcessIo(ProcessIoError),
    ProcessStatus(ProcessStatusError),
    Io(io::Error),
    Ctrlc(ctrlc::Error),
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ExecutionError::ProcessIo(ref err) => write!(f, "{}", err),
            ExecutionError::ProcessStatus(ref err) => write!(f, "{}", err),
            ExecutionError::Io(ref err) => write!(f, "IO error: {}", err),
            ExecutionError::Ctrlc(ref err) => write!(f, "{}", err),
        }
    }
}

impl error::Error for ExecutionError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match *self {
            ExecutionError::ProcessIo(ref err) => Some(err),
            ExecutionError::ProcessStatus(ref err) => Some(err),
            ExecutionError::Io(ref err) => Some(err),
            ExecutionError::Ctrlc(ref err) => Some(err),
        }
    }
}

impl From<ProcessIoError> for ExecutionError {
    fn from(error: ProcessIoError) -> Self {
        ExecutionError::ProcessIo(error)
    }
}

impl From<ProcessStatusError> for ExecutionError {
    fn from(error: ProcessStatusError) -> Self {
        ExecutionError::ProcessStatus(error)
    }
}

impl From<io::Error> for ExecutionError {
    fn from(error: io::Error) -> Self {
        ExecutionError::Io(error)
    }
}

impl From<ctrlc::Error> for ExecutionError {
    fn from(error: ctrlc::Error) -> Self {
        ExecutionError::Ctrlc(error)
    }
}

#[derive(Debug)]
pub struct ProcessIoError {
    cmd: ProcessCommand,
    error: io::Error,
}

impl ProcessIoError {
    pub fn new(cmd: ProcessCommand, error: io::Error) -> Self {
        Self { cmd, error }
    }
}

impl error::Error for ProcessIoError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.error)
    }
}

impl fmt::Display for ProcessIoError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Command {} failed: {}", self.cmd, self.error)
    }
}

#[derive(Debug)]
pub struct ProcessStatusError {
    cmd: ProcessCommand,
    termination: Termination,
}

impl ProcessStatusError {
    pub fn new(cmd: ProcessCommand, status: ExitStatus) -> Self {
        Self {
            cmd,
            termination: Termination::from(status),
        }
    }
}

impl fmt::Display for ProcessStatusError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.termination {
            Termination::Value(code) => {
                write!(f, "Command {} returned with value {}", self.cmd, code)
            }
            #[cfg(unix)]
            Termination::Signal(sig) => {
                write!(f, "Command {} terminated by signal {}", self.cmd, sig)
            }
        }
    }
}

impl error::Error for ProcessStatusError {}

#[derive(Debug)]
pub struct ProcessCommand {
    command: String,
    args: Vec<String>,
}

impl ProcessCommand {
    pub fn new<S1, S2>(command: S1, args: Vec<S2>) -> ProcessCommand
    where
        S1: Into<String>,
        S2: Into<String>,
    {
        Self {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect::<Vec<_>>(),
        }
    }
}

impl fmt::Display for ProcessCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?}", self.command, self.args)
    }
}

#[derive(Debug)]
pub enum Termination {
    Value(i32),
    #[cfg(unix)]
    Signal(i32),
}

impl Termination {
    fn from(status: ExitStatus) -> Self {
        match status.code() {
            Some(code) => Termination::Value(code),
            #[cfg(unix)]
            None => Termination::Signal(status.signal().unwrap()),
            #[cfg(not(unix))]
            None => panic!("Unexpected signal termination on non-unix system"),
        }
    }
}
