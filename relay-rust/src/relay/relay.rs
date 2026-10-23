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

use crate::{debug, info};
use mio::Events;
use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::selector::Selector;
use super::tunnel_server::TunnelServer;
use super::udp_connection::IDLE_TIMEOUT_SECONDS;

const TAG: &str = "Relay";
const CLEANING_INTERVAL: Duration = Duration::from_secs(60);

pub struct Relay {
    port: u16,
}

impl Relay {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    pub fn run(&self) -> io::Result<()> {
        let mut selector = Selector::create()?;
        let tunnel_server = TunnelServer::create(self.port, &mut selector)?;
        info!(target: TAG, "Relay server started");
        self.poll_loop(&mut selector, &tunnel_server)
    }

    fn poll_loop(
        &self,
        selector: &mut Selector,
        tunnel_server: &Rc<RefCell<TunnelServer>>,
    ) -> io::Result<()> {
        let mut events = Events::with_capacity(1024);
        // no connection may expire before the UDP idle timeout delay
        let mut next_cleaning_deadline = Instant::now() + Duration::from_secs(IDLE_TIMEOUT_SECONDS);
        loop {
            retry_on_intr!({
                let timeout =
                    Some(next_cleaning_deadline.saturating_duration_since(Instant::now()));
                selector.poll(&mut events, timeout)
            })?;

            let now = Instant::now();
            if now >= next_cleaning_deadline {
                tunnel_server.borrow_mut().clean_up(selector);
                next_cleaning_deadline = now + CLEANING_INTERVAL;
            } else if events.is_empty() {
                debug!(
                    target: TAG,
                    "Spurious wakeup: poll() returned without any event"
                );
                continue;
            }

            selector.run_handlers(&events);
        }
    }
}
