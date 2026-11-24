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

use std::net::IpAddr;

use clap::{Parser, Subcommand};

/// Port used by the relay server and the adb reverse tunnel.
pub const DEFAULT_PORT: u16 = 31416;
/// DNS server used when the client does not specify any.
pub const DEFAULT_DNS_SERVERS: &str = "8.8.8.8";
/// Routes used when the client does not specify any (IPv4 only).
///
/// IPv6 is opt-in, because capturing it on a host without working IPv6
/// connectivity would black-hole IPv6-preferring traffic.
pub const DEFAULT_ROUTES: &str = "0.0.0.0/0";

/// Route added by `-6`/`--ipv6` when no IPv6 route is already configured.
pub const IPV6_DEFAULT_ROUTE: &str = "::/0";

/// Whether the route list contains at least one IPv6 route.
pub fn contains_ipv6_route(routes: &str) -> bool {
    routes.split(',').any(|route| {
        let address = route.trim().split_once('/').map_or(route, |(a, _)| a);
        address.contains(':')
    })
}

/// Append the default IPv6 route when IPv6 is requested and not already
/// covered by an explicit route.
pub fn enable_ipv6(routes: &str, ipv6: bool) -> String {
    if ipv6 && !contains_ipv6_route(routes) {
        format!("{routes},{IPV6_DEFAULT_ROUTE}")
    } else {
        routes.to_string()
    }
}

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

        /// Also reverse tether IPv6 traffic (::/0).
        ///
        /// This requires working IPv6 connectivity on the host.
        #[arg(short = '6', long = "ipv6")]
        ipv6: bool,
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

        /// Also reverse tether IPv6 traffic (::/0).
        ///
        /// This requires working IPv6 connectivity on the host.
        #[arg(short = '6', long = "ipv6")]
        ipv6: bool,
    },

    /// Start a client on the Android device and exit.
    ///
    /// If several devices are connected via adb, then serial must be specified.
    ///
    /// If -d is given, then make the Android device use the specified DNS
    /// server(s). Otherwise, use 8.8.8.8 (Google public DNS).
    ///
    /// If -r is given, then only reverse tether the specified routes.
    /// Otherwise, use 0.0.0.0/0 (redirect the whole IPv4 traffic). Pass -6 to
    /// also redirect IPv6 traffic (::/0); it requires working IPv6
    /// connectivity on the host.
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

        /// Also reverse tether IPv6 traffic (::/0).
        ///
        /// This requires working IPv6 connectivity on the host.
        #[arg(short = '6', long = "ipv6")]
        ipv6: bool,
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

        /// Also reverse tether IPv6 traffic (::/0).
        ///
        /// This requires working IPv6 connectivity on the host.
        #[arg(short = '6', long = "ipv6")]
        ipv6: bool,
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

        /// Also reverse tether IPv6 traffic (::/0).
        ///
        /// This requires working IPv6 connectivity on the host.
        #[arg(short = '6', long = "ipv6")]
        ipv6: bool,
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
            .parse::<IpAddr>()
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
            let address: IpAddr = address
                .parse()
                .map_err(|_| format!("invalid route address: {route}"))?;
            let max_prefix = if address.is_ipv4() { 32 } else { 128 };
            if prefix > max_prefix {
                return Err(format!("invalid route prefix: {route}"));
            }
        } else {
            route
                .parse::<IpAddr>()
                .map_err(|_| format!("invalid route address: {route}"))?;
        }
        routes.push(route);
    }
    Ok(routes.join(","))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

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

    #[test]
    fn default_routes_are_ipv4_only() {
        let cli = Cli::try_parse_from(["gnirehtet", "run"]).unwrap();
        match cli.cmd {
            Command::Run { routes, ipv6, .. } => {
                assert_eq!("0.0.0.0/0", routes);
                assert!(!ipv6);
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn ipv6_flag_adds_default_route() {
        let cli = Cli::try_parse_from(["gnirehtet", "run", "-6"]).unwrap();
        match cli.cmd {
            Command::Run { routes, ipv6, .. } => {
                assert!(ipv6);
                assert_eq!("0.0.0.0/0,::/0", super::enable_ipv6(&routes, ipv6));
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn ipv6_flag_is_available_on_all_client_commands() {
        for command in ["run", "autorun", "start", "autostart", "restart"] {
            let cli = Cli::try_parse_from(["gnirehtet", command, "--ipv6"]).unwrap();
            match cli.cmd {
                Command::Run { ipv6, .. }
                | Command::Autorun { ipv6, .. }
                | Command::Start { ipv6, .. }
                | Command::Autostart { ipv6, .. }
                | Command::Restart { ipv6, .. } => assert!(ipv6, "{command} should accept --ipv6"),
                _ => panic!("unexpected command {command}"),
            }
        }
    }

    #[test]
    fn explicit_ipv6_route_is_not_duplicated() {
        assert_eq!(
            "0.0.0.0/0,fd00::/8",
            super::enable_ipv6("0.0.0.0/0,fd00::/8", true)
        );
        assert_eq!("0.0.0.0/0,::/0", super::enable_ipv6("0.0.0.0/0,::/0", true));
        assert_eq!("0.0.0.0/0", super::enable_ipv6("0.0.0.0/0", false));
    }

    #[test]
    fn ipv6_routes_and_dns_are_accepted() {
        let cli = Cli::try_parse_from([
            "gnirehtet",
            "run",
            "-r",
            "0.0.0.0/0,fd00::/8",
            "-d",
            "8.8.8.8,2001:4860:4860::8888",
        ])
        .unwrap();
        match cli.cmd {
            Command::Run {
                routes,
                dns_servers,
                ..
            } => {
                assert_eq!("0.0.0.0/0,fd00::/8", routes);
                assert_eq!("8.8.8.8,2001:4860:4860::8888", dns_servers);
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn rejects_ipv6_prefix_above_128() {
        assert!(Cli::try_parse_from(["gnirehtet", "run", "-r", "fd00::/129"]).is_err());
    }
}
