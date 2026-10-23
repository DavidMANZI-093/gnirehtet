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

use std::net::Ipv4Addr;

use clap::{Parser, Subcommand};

/// Port used by the relay server and the adb reverse tunnel.
pub const DEFAULT_PORT: u16 = 31416;
/// DNS server used when the client does not specify any.
pub const DEFAULT_DNS_SERVERS: &str = "8.8.8.8";
/// Route used when the client does not specify any.
pub const DEFAULT_ROUTES: &str = "0.0.0.0/0";

#[derive(Parser)]
#[command(
    name = "gnirehtet",
    version,
    max_term_width = 80,
    about = "gnirehtet provides reverse tethering for android."
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,

    /// Enable debug logging.
    #[arg(short = 'v', long = "verbose", global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Install the client on the Android device and exit.
    ///
    /// If several devices are connected via adb, then serial must be specified.
    Install {
        /// Device serial.
        #[arg(value_name = "SERIAL")]
        serial: Option<String>,
    },

    /// Uninstall the client from the Android device and exit.
    ///
    /// If several devices are connected via adb, then serial must be specified.
    Uninstall {
        /// Device serial.
        #[arg(value_name = "SERIAL")]
        serial: Option<String>,
    },

    /// Uninstall then install.
    Reinstall {
        /// Device serial.
        #[arg(value_name = "SERIAL")]
        serial: Option<String>,
    },

    /// Enable reverse tethering for exactly one device.
    ///
    /// - install the client if necessary;
    /// - start the client;
    /// - start the relay server;
    /// - on Ctrl+C, stop both the relay server and the client.
    Run {
        /// Device serial.
        #[arg(value_name = "SERIAL")]
        serial: Option<String>,

        /// Comma-separated DNS server(s) to use on the device.
        #[arg(
            short = 'd',
            value_name = "DNS[,DNS2,...]",
            default_value = DEFAULT_DNS_SERVERS,
            value_parser = parse_dns_servers
        )]
        dns_servers: String,

        /// Comma-separated route(s) to reverse tether.
        #[arg(
            short = 'r',
            value_name = "ROUTE[,ROUTE2,...]",
            default_value = DEFAULT_ROUTES,
            value_parser = parse_routes
        )]
        routes: String,

        /// Relay server port.
        #[arg(
            short = 'p',
            value_name = "PORT",
            default_value_t = DEFAULT_PORT,
            value_parser = parse_port
        )]
        port: u16,
    },

    /// Enable reverse tethering for all devices.
    ///
    /// - monitor devices and start clients (autostart);
    /// - start the relay server.
    Autorun {
        /// Comma-separated DNS server(s) to use on the device.
        #[arg(
            short = 'd',
            value_name = "DNS[,DNS2,...]",
            default_value = DEFAULT_DNS_SERVERS,
            value_parser = parse_dns_servers
        )]
        dns_servers: String,

        /// Comma-separated route(s) to reverse tether.
        #[arg(
            short = 'r',
            value_name = "ROUTE[,ROUTE2,...]",
            default_value = DEFAULT_ROUTES,
            value_parser = parse_routes
        )]
        routes: String,

        /// Relay server port.
        #[arg(
            short = 'p',
            value_name = "PORT",
            default_value_t = DEFAULT_PORT,
            value_parser = parse_port
        )]
        port: u16,
    },

    /// Start a client on the Android device and exit.
    ///
    /// If several devices are connected via adb, then serial must be specified.
    ///
    /// If -d is given, then make the Android device use the specified DNS
    /// server(s). Otherwise, use 8.8.8.8 (Google public DNS).
    ///
    /// If -r is given, then only reverse tether the specified routes.
    /// Otherwise, use 0.0.0.0/0 (redirect the whole traffic).
    ///
    /// If -p is given, then make the relay server listen on the specified
    /// port. Otherwise, use port 31416.
    ///
    /// If the client is already started, then do nothing, and ignore the other
    /// parameters.
    ///
    /// 10.0.2.2 is mapped to the host 'localhost'.
    Start {
        /// Device serial.
        #[arg(value_name = "SERIAL")]
        serial: Option<String>,

        /// Comma-separated DNS server(s) to use on the device.
        #[arg(
            short = 'd',
            value_name = "DNS[,DNS2,...]",
            default_value = DEFAULT_DNS_SERVERS,
            value_parser = parse_dns_servers
        )]
        dns_servers: String,

        /// Comma-separated route(s) to reverse tether.
        #[arg(
            short = 'r',
            value_name = "ROUTE[,ROUTE2,...]",
            default_value = DEFAULT_ROUTES,
            value_parser = parse_routes
        )]
        routes: String,

        /// Relay server port.
        #[arg(
            short = 'p',
            value_name = "PORT",
            default_value_t = DEFAULT_PORT,
            value_parser = parse_port
        )]
        port: u16,
    },

    /// Listen for device connections and start a client on every detected
    /// device.
    ///
    /// Accept the same parameters as the start command (excluding the serial,
    /// which will be taken from the detected device).
    Autostart {
        /// Comma-separated DNS server(s) to use on the device.
        #[arg(
            short = 'd',
            value_name = "DNS[,DNS2,...]",
            default_value = DEFAULT_DNS_SERVERS,
            value_parser = parse_dns_servers
        )]
        dns_servers: String,

        /// Comma-separated route(s) to reverse tether.
        #[arg(
            short = 'r',
            value_name = "ROUTE[,ROUTE2,...]",
            default_value = DEFAULT_ROUTES,
            value_parser = parse_routes
        )]
        routes: String,

        /// Relay server port.
        #[arg(
            short = 'p',
            value_name = "PORT",
            default_value_t = DEFAULT_PORT,
            value_parser = parse_port
        )]
        port: u16,
    },

    /// Stop the client on the Android device and exit.
    ///
    /// If several devices are connected via adb, then serial must be specified.
    Stop {
        /// Device serial.
        #[arg(value_name = "SERIAL")]
        serial: Option<String>,
    },

    /// Stop then start.
    Restart {
        /// Device serial.
        #[arg(value_name = "SERIAL")]
        serial: Option<String>,

        /// Comma-separated DNS server(s) to use on the device.
        #[arg(
            short = 'd',
            value_name = "DNS[,DNS2,...]",
            default_value = DEFAULT_DNS_SERVERS,
            value_parser = parse_dns_servers
        )]
        dns_servers: String,

        /// Comma-separated route(s) to reverse tether.
        #[arg(
            short = 'r',
            value_name = "ROUTE[,ROUTE2,...]",
            default_value = DEFAULT_ROUTES,
            value_parser = parse_routes
        )]
        routes: String,

        /// Relay server port.
        #[arg(
            short = 'p',
            value_name = "PORT",
            default_value_t = DEFAULT_PORT,
            value_parser = parse_port
        )]
        port: u16,
    },

    /// Set up the 'adb reverse' tunnel.
    ///
    /// If a device is unplugged then plugged back while gnirehtet is active,
    /// resetting the tunnel is sufficient to get the connection back.
    Tunnel {
        /// Device serial.
        #[arg(value_name = "SERIAL")]
        serial: Option<String>,

        /// Relay server port.
        #[arg(
            short = 'p',
            value_name = "PORT",
            default_value_t = DEFAULT_PORT,
            value_parser = parse_port
        )]
        port: u16,
    },

    /// Start the relay server in the current terminal.
    Relay {
        /// Relay server port.
        #[arg(
            short = 'p',
            value_name = "PORT",
            default_value_t = DEFAULT_PORT,
            value_parser = parse_port
        )]
        port: u16,
    },
}

// ---------- parsers ----------

fn parse_port(value: &str) -> Result<u16, String> {
    let port: u16 = value
        .parse()
        .map_err(|_| format!("invalid port: {value}"))?;
    if port == 0 {
        Err("port must be greater than 0".to_string())
    } else {
        Ok(port)
    }
}

fn parse_dns_servers(value: &str) -> Result<String, String> {
    let mut servers = Vec::new();
    for server in value.split(',') {
        let server = server.trim();
        if server.is_empty() {
            return Err("DNS server list contains an empty entry".to_string());
        }
        server
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("invalid DNS server: {server}"))?;
        servers.push(server);
    }
    Ok(servers.join(","))
}

fn parse_routes(value: &str) -> Result<String, String> {
    let mut routes = Vec::new();
    for route in value.split(',') {
        let route = route.trim();
        if route.is_empty() {
            return Err("route list contains an empty entry".to_string());
        }
        if let Some((address, prefix)) = route.split_once('/') {
            let prefix: u8 = prefix
                .parse()
                .map_err(|_| format!("invalid route prefix: {route}"))?;
            if prefix > 32 {
                return Err(format!("invalid route prefix: {route}"));
            }
            address
                .parse::<Ipv4Addr>()
                .map_err(|_| format!("invalid route address: {route}"))?;
        } else {
            route
                .parse::<Ipv4Addr>()
                .map_err(|_| format!("invalid route address: {route}"))?;
        }
        routes.push(route);
    }
    Ok(routes.join(","))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn verbose_defaults_to_false() {
        let cli = Cli::try_parse_from(["gnirehtet", "relay"]).unwrap();
        assert!(!cli.verbose);
    }

    #[test]
    fn verbose_is_global() {
        let before = Cli::try_parse_from(["gnirehtet", "-v", "relay"]).unwrap();
        assert!(before.verbose);

        let after = Cli::try_parse_from(["gnirehtet", "relay", "--verbose"]).unwrap();
        assert!(after.verbose);
    }
}
