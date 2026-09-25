use std::{
    collections::HashSet,
    fmt,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use hidapi::{HidApi, HidDevice};

const HYPERX_VENDOR_ID: u16 = 0x03f0;
const CLOUD_III_S_PRODUCT_IDS: [u16; 2] = [0x06be, 0x02cc];

const REPORT_ID: u8 = 0x0c;
const RESPONSE_ID: u8 = 0x0c;
const NOTIFICATION_ID: u8 = 0x0d;
const CONNECTED_COMMAND_ID: u8 = 0x02;
const CONNECTED_NOTIFICATION_ID: u8 = 12;

const REPORT_SIZE: usize = 64;
const RESPONSE_BUFFER_SIZE: usize = 256;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_STALE_REPORTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadsetState {
    Connected,
    Disconnected,
}

impl HeadsetState {
    pub(crate) fn is_connected(self) -> bool {
        matches!(self, Self::Connected)
    }
}

impl fmt::Display for HeadsetState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected => formatter.write_str("connected"),
            Self::Disconnected => formatter.write_str("disconnected"),
        }
    }
}

#[derive(Default)]
pub(crate) struct HeadsetProbe {
    device: Option<HidDevice>,
}

impl HeadsetProbe {
    pub(crate) fn query(&mut self) -> Result<HeadsetState> {
        if let Some(device) = self.device.as_ref() {
            match query_connected(device) {
                Ok(state) => return Ok(state),
                Err(query_error) => {
                    // Drop the stale handle before trying to reopen the dongle.
                    self.device = None;

                    return match open_responding_device() {
                        Ok((device, state)) => {
                            self.device = Some(device);
                            Ok(state)
                        }
                        Err(reopen_error) => Err(anyhow!(
                            "lost HyperX HID connection: {query_error:#}; reconnect failed: {reopen_error:#}"
                        )),
                    };
                }
            }
        }

        let (device, state) = open_responding_device()?;
        self.device = Some(device);
        Ok(state)
    }
}

fn open_responding_device() -> Result<(HidDevice, HeadsetState)> {
    let api = HidApi::new().context("failed to initialize hidapi")?;
    let mut seen_paths = HashSet::new();
    let mut found = 0usize;
    let mut failures = Vec::new();

    for info in api.device_list().filter(|info| {
        info.vendor_id() == HYPERX_VENDOR_ID && CLOUD_III_S_PRODUCT_IDS.contains(&info.product_id())
    }) {
        // hidapi can expose the same hidraw path more than once for different
        // HID collections. Opening it repeatedly only duplicates errors.
        if !seen_paths.insert(info.path().to_bytes().to_owned()) {
            continue;
        }

        found += 1;
        let label = format!(
            "{:04x}:{:04x} {}",
            info.vendor_id(),
            info.product_id(),
            info.path().to_string_lossy()
        );

        let device = match info.open_device(&api) {
            Ok(device) => device,
            Err(error) => {
                failures.push(format!("{label}: open failed: {error}"));
                continue;
            }
        };

        match query_connected(&device) {
            Ok(state) => return Ok((device, state)),
            Err(error) => failures.push(format!("{label}: no valid response: {error:#}")),
        }
    }

    if found == 0 {
        bail!("HyperX Cloud III S dongle not found (expected 03f0:06be or 03f0:02cc)");
    }

    bail!(
        "found {found} matching HID path(s), but none responded: {}",
        failures.join("; ")
    )
}

fn query_connected(device: &HidDevice) -> Result<HeadsetState> {
    drain_pending_reports(device)?;

    let mut packet = [0u8; REPORT_SIZE];
    packet[0] = REPORT_ID;
    packet[1] = 0x02;
    packet[2] = 0x03;
    packet[3] = 0x01;
    packet[4] = 0x00;
    packet[5] = CONNECTED_COMMAND_ID;

    let written = device
        .write(&packet)
        .context("failed to write connected-state HID request")?;
    if written != packet.len() {
        bail!(
            "short connected-state HID write: wrote {written} of {} bytes",
            packet.len()
        );
    }

    // read_timeout already blocks until a response arrives, so a fixed sleep
    // before it only adds latency.
    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    let mut response = [0u8; RESPONSE_BUFFER_SIZE];

    loop {
        let now = Instant::now();
        if now >= deadline {
            bail!("timed out waiting for connected-state HID response");
        }

        let remaining_ms = (deadline - now).as_millis().clamp(1, i32::MAX as u128) as i32;
        let len = device
            .read_timeout(&mut response, remaining_ms)
            .context("failed to read HID response")?;

        if len == 0 {
            bail!("timed out waiting for connected-state HID response");
        }

        if let Some(state) = parse_connected_state(&response[..len]) {
            return Ok(state);
        }
    }
}

fn drain_pending_reports(device: &HidDevice) -> Result<()> {
    let mut buffer = [0u8; RESPONSE_BUFFER_SIZE];
    // A noisy device must not monopolize the daemon indefinitely.
    for _ in 0..MAX_STALE_REPORTS {
        let len = device
            .read_timeout(&mut buffer, 0)
            .context("failed while draining stale HID reports")?;
        if len == 0 {
            return Ok(());
        }
    }
    bail!("HID report queue did not drain after {MAX_STALE_REPORTS} reports")
}

fn parse_connected_state(response: &[u8]) -> Option<HeadsetState> {
    if response.len() >= 7 && response[0] == RESPONSE_ID && response[5] == CONNECTED_COMMAND_ID {
        return match response[6] {
            0 => Some(HeadsetState::Disconnected),
            2 => Some(HeadsetState::Connected),
            _ => None,
        };
    }

    if response.len() >= 6
        && response[0] == NOTIFICATION_ID
        && response[4] == CONNECTED_NOTIFICATION_ID
    {
        return match response[5] {
            0 => Some(HeadsetState::Disconnected),
            1 => Some(HeadsetState::Connected),
            _ => None,
        };
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{parse_connected_state, HeadsetState};

    #[test]
    fn parses_connected_query_response() {
        let response = [0x0c, 0, 0, 0, 0, 0x02, 0x02];
        assert_eq!(
            parse_connected_state(&response),
            Some(HeadsetState::Connected)
        );
    }

    #[test]
    fn parses_disconnected_query_response() {
        let response = [0x0c, 0, 0, 0, 0, 0x02, 0x00];
        assert_eq!(
            parse_connected_state(&response),
            Some(HeadsetState::Disconnected)
        );
    }

    #[test]
    fn ignores_unknown_query_response() {
        let response = [0x0c, 0, 0, 0, 0, 0x02, 0xff];
        assert_eq!(parse_connected_state(&response), None);
    }

    #[test]
    fn parses_connected_notification() {
        let response = [0x0d, 0, 0, 0, 12, 1];
        assert_eq!(
            parse_connected_state(&response),
            Some(HeadsetState::Connected)
        );
    }

    #[test]
    fn parses_disconnected_notification() {
        let response = [0x0d, 0, 0, 0, 12, 0];
        assert_eq!(
            parse_connected_state(&response),
            Some(HeadsetState::Disconnected)
        );
    }

    #[test]
    fn ignores_unknown_notification_response() {
        let response = [0x0d, 0, 0, 0, 12, 2];
        assert_eq!(parse_connected_state(&response), None);
    }

    #[test]
    fn ignores_unrelated_reports() {
        let response = [0x0d, 0, 0, 0, 3, 1];
        assert_eq!(parse_connected_state(&response), None);
    }
}
