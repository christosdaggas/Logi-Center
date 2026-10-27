use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use thiserror::Error;

use crate::hidraw::Hidraw;

pub const HIDPP_SHORT_REPORT: u8 = 0x10;
pub const HIDPP_LONG_REPORT: u8 = 0x11;
pub const LOGICENTER_SOFTWARE_ID: u8 = 0x0B;

pub const RECEIVER_DEVICE_NUMBER: u8 = 0xFF;

#[derive(Debug, Error)]
pub enum HidppError {
    #[error("HID++ device error 0x{0:02X}")]
    Device(u8),
    #[error("malformed HID++ reply")]
    Malformed,
    #[error("HID++ request timed out (device {device:02X}, request {request:04X})")]
    Timeout { device: u8, request: u16 },
}

/// Injectable packet transport. Replay tests need neither /dev/hidraw nor hardware.
pub trait Transport: Send {
    fn write_report(&mut self, report: &[u8]) -> Result<()>;
    fn read_report(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>>;
    fn drain(&mut self) -> Result<()> {
        for _ in 0..64 {
            if self.read_report(Duration::ZERO)?.is_none() {
                break;
            }
        }
        Ok(())
    }
}

impl Transport for Hidraw {
    fn write_report(&mut self, report: &[u8]) -> Result<()> {
        Hidraw::write_report(self, report)
    }
    fn read_report(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        Hidraw::read_report(self, timeout)
    }
}

#[derive(Clone, Debug)]
pub struct HidppNotification {
    pub device: u8,
    pub sub_id: u8,
    pub address: u8,
    pub data: Vec<u8>,
}

/// A HID++ 1.0/2.0 connection over one hidraw node.
pub struct Hidpp {
    io: Box<dyn Transport>,
    diagnostics: Vec<String>,
}

impl Hidpp {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self::with_transport(Hidraw::open(path)?))
    }

    pub fn with_transport(transport: impl Transport + 'static) -> Self {
        Self {
            io: Box::new(transport),
            diagnostics: Vec::new(),
        }
    }

    pub fn take_diagnostics(&mut self) -> Vec<String> {
        std::mem::take(&mut self.diagnostics)
    }

    /// Send a raw HID++ request and wait for its reply.
    ///
    /// The returned bytes are the payload after the two-byte
    /// feature/function (or register) response header, matching the reference
    /// Python implementation's low-level API.
    fn request(
        &mut self,
        device: u8,
        request_id: u16,
        params: &[u8],
        long: bool,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        self.io.drain()?;

        let report_size = if long { 20 } else { 7 };
        if params.len() > report_size - 4 {
            bail!("HID++ request payload exceeds report capacity");
        }
        let mut report = vec![0u8; report_size];
        report[0] = if long {
            HIDPP_LONG_REPORT
        } else {
            HIDPP_SHORT_REPORT
        };
        report[1] = device;
        report[2] = (request_id >> 8) as u8;
        report[3] = request_id as u8;
        let count = params.len().min(report_size - 4);
        report[4..4 + count].copy_from_slice(&params[..count]);
        log::debug!("HID++ write={report:02X?}");
        self.io.write_report(&report)?;

        let deadline = Instant::now() + timeout;
        log::debug!(
            "HID++ request dev={device:02X} id={request_id:04X} long={long} params={params:02X?}"
        );
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let Some(raw) = self.io.read_report(deadline - now)? else {
                return Ok(None);
            };
            if raw.len() < 4 || !matches!(raw[0], HIDPP_SHORT_REPORT | HIDPP_LONG_REPORT) {
                continue;
            }
            let reply_device = raw[1];
            if reply_device != device
                && !(matches!(device, 0 | 0xFF) && reply_device == (device ^ 0xFF))
            {
                continue;
            }
            let data = &raw[2..];
            log::debug!(
                "HID++ reply dev={reply_device:02X} report={:02X} data={data:02X?}",
                raw[0]
            );
            if data.len() < 2 {
                continue;
            }

            if let Some(code) = reply_error(request_id, data) {
                return Err(HidppError::Device(code).into());
            }

            let high_matches = data[0] == (request_id >> 8) as u8;
            let low_matches = data[1] == (request_id & 0xFF) as u8;
            // Some devices omit the software id nibble in replies. Accept
            // that form as well, but keep the high byte strict.
            let low_nibble_matches = request_id >> 8 < 0x80
                && data[1] & 0x0F == 0
                && (data[1] & 0xF0) == (request_id as u8 & 0xF0);
            if high_matches && (low_matches || low_nibble_matches) {
                return Ok(Some(data[2..].to_vec()));
            }
        }
    }

    /// Ping a paired device. This is important for devices that go into a
    /// low-power state: the receiver can report them as paired while they do
    /// not answer feature requests until they have been woken.
    pub fn ping(&mut self, device: u8) -> Result<bool> {
        let request_id = 0x0010 | LOGICENTER_SOFTWARE_ID as u16;
        for attempt in 0..3u8 {
            let reply = self.request(
                device,
                request_id,
                &[0x00, 0x00, 0x50 + attempt],
                true,
                Duration::from_millis(500),
            )?;
            if reply
                .as_ref()
                .is_some_and(|reply| reply.get(2) == Some(&(0x50 + attempt)))
            {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        Ok(false)
    }

    /// Read a HID++ 1.0 register from the receiver.
    pub fn read_register(&mut self, register: u16, params: &[u8]) -> Result<Option<Vec<u8>>> {
        let request_id = 0x8100 | (register & 0x02FF);
        self.request(
            RECEIVER_DEVICE_NUMBER,
            request_id,
            params,
            false,
            Duration::from_millis(450),
        )
    }

    /// Write a HID++ 1.0 register on the receiver. Long registers use the
    /// request-id form where the register is prefixed with 0x200.
    pub fn write_register(&mut self, register: u16, params: &[u8]) -> Result<bool> {
        let request_id = 0x8000 | (register & 0x02FF);
        let long = register & 0x200 != 0 || params.len() > 3;
        Ok(self
            .request(
                RECEIVER_DEVICE_NUMBER,
                request_id,
                params,
                long,
                Duration::from_millis(450),
            )?
            .is_some())
    }

    /// Wait for the next unsolicited HID++ 1.0 notification (sub-id 0x40-0x7F)
    /// from the receiver. Regular request replies are skipped.
    pub fn read_notification(&mut self, timeout: Duration) -> Result<Option<HidppNotification>> {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let Some(raw) = self.io.read_report(deadline - now)? else {
                return Ok(None);
            };
            if raw.len() < 4 {
                continue;
            }
            let built = &raw[2..];
            let sub_id = built[0];
            if !(0x40..=0x7F).contains(&sub_id) {
                continue;
            }
            log::debug!(
                "HID++ notification dev={:02X} sub={sub_id:02X} address={:02X} data={:02X?}",
                raw[1],
                built[1],
                &built[2..]
            );
            return Ok(Some(HidppNotification {
                device: raw[1],
                sub_id,
                address: built[1],
                data: built[2..].to_vec(),
            }));
        }
    }

    /// Send a HID++ 2.0 feature request.
    pub fn feature_request(
        &mut self,
        device: u8,
        feature_index: u8,
        function: u8,
        params: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let request_id = ((feature_index as u16) << 8)
            | ((function & 0xF0) as u16)
            | LOGICENTER_SOFTWARE_ID as u16;
        self.request(device, request_id, params, true, Duration::from_millis(500))
    }

    /// Send a feature request without waiting for its acknowledgement. This is
    /// required for operations such as host switching that intentionally make
    /// the current connection disappear.
    pub fn send_feature(
        &mut self,
        device: u8,
        feature_index: u8,
        function: u8,
        params: &[u8],
    ) -> Result<()> {
        let request_id = ((feature_index as u16) << 8)
            | ((function & 0xF0) as u16)
            | LOGICENTER_SOFTWARE_ID as u16;
        self.request(device, request_id, params, true, Duration::ZERO)?;
        Ok(())
    }

    /// Resolve a feature id (for example 0x2201) to its per-device index.
    pub fn feature_index(&mut self, device: u8, feature_id: u16) -> Result<Option<u8>> {
        let reply = self
            .feature_request(device, 0, 0, &feature_id.to_be_bytes())?
            .ok_or(HidppError::Timeout {
                device,
                request: LOGICENTER_SOFTWARE_ID as u16,
            })?;
        let index = reply.first().copied().ok_or(HidppError::Malformed)?;
        Ok(Some(index).filter(|index| *index != 0))
    }

    /// Compatibility convenience reader. Errors are retained for the scanner;
    /// callers needing precise failure handling use `read_feature_checked`.
    pub fn read_feature(
        &mut self,
        device: u8,
        feature_index: u8,
        function: u8,
        params: &[u8],
    ) -> Option<Vec<u8>> {
        match self.read_feature_checked(device, feature_index, function, params) {
            Ok(reply) => Some(reply),
            Err(error) => {
                self.diagnostics.push(format!(
                    "feature {feature_index:02X}/{function:02X}: {error:#}"
                ));
                None
            }
        }
    }

    pub fn read_feature_checked(
        &mut self,
        device: u8,
        index: u8,
        function: u8,
        params: &[u8],
    ) -> Result<Vec<u8>> {
        self.feature_request(device, index, function, params)?
            .ok_or_else(|| {
                HidppError::Timeout {
                    device,
                    request: ((index as u16) << 8)
                        | function as u16
                        | LOGICENTER_SOFTWARE_ID as u16,
                }
                .into()
            })
    }

    pub fn record_diagnostic(&mut self, message: String) {
        self.diagnostics.push(message);
    }

    pub fn write_feature(
        &mut self,
        device: u8,
        feature_index: u8,
        function: u8,
        params: &[u8],
    ) -> Result<()> {
        self.read_feature_checked(device, feature_index, function, params)?;
        Ok(())
    }
}

fn reply_error(request_id: u16, data: &[u8]) -> Option<u8> {
    // HID++ 1.0 uses 0x8F, while HID++ 2.0 uses 0xFF.
    let [kind, high, low, code, ..] = data else {
        return None;
    };
    (matches!(kind, 0x8F | 0xFF) && *high == (request_id >> 8) as u8 && *low == request_id as u8)
        .then_some(*code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct Replay {
        packets: VecDeque<Vec<u8>>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }
    impl Transport for Replay {
        fn write_report(&mut self, report: &[u8]) -> Result<()> {
            self.writes.lock().unwrap().push(report.to_vec());
            Ok(())
        }
        fn read_report(&mut self, _: Duration) -> Result<Option<Vec<u8>>> {
            Ok(self.packets.pop_front())
        }
        fn drain(&mut self) -> Result<()> {
            Ok(())
        }
    }
    fn replay(packets: Vec<Vec<u8>>) -> Hidpp {
        Hidpp::with_transport(Replay {
            packets: packets.into(),
            writes: Default::default(),
        })
    }

    #[test]
    fn replay_ignores_wrong_device_software_id_and_register_low_byte() {
        let mut hp = replay(vec![
            vec![0x11, 2, 3, 0x1B, 1],
            vec![0x11, 1, 3, 0x1A, 2],
            vec![0x11, 1, 3, 0x1B, 42],
        ]);
        assert_eq!(hp.read_feature_checked(1, 3, 0x10, &[]).unwrap(), [42]);
        let mut hp = replay(vec![
            vec![0x11, 0xFF, 0x83, 0xB4, 1],
            vec![0x11, 0xFF, 0x83, 0xB5, 7],
        ]);
        assert_eq!(hp.read_register(0x2B5, &[]).unwrap().unwrap(), [7]);
    }

    #[test]
    fn replay_missing_feature_is_distinct_from_timeout_and_protocol_error() {
        let mut absent = replay(vec![vec![0x11, 1, 0, 0x0B, 0, 0, 0]]);
        assert_eq!(absent.feature_index(1, 0x2201).unwrap(), None);
        let error = replay(vec![]).feature_index(1, 0x2201).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<HidppError>(),
            Some(HidppError::Timeout { .. })
        ));
        let error = replay(vec![vec![0x11, 1, 0xFF, 0, 0x0B, 8]])
            .feature_index(1, 0x2201)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<HidppError>(),
            Some(HidppError::Device(8))
        ));
    }

    #[test]
    fn oversized_payload_is_rejected_without_silent_truncation_or_write() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut hp = Hidpp::with_transport(Replay {
            packets: Default::default(),
            writes: writes.clone(),
        });
        assert!(hp.write_feature(1, 2, 0x10, &[0; 17]).is_err());
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn legacy_option_reader_keeps_failure_diagnostics() {
        let mut hp = replay(vec![]);
        assert!(hp.read_feature(1, 2, 0x10, &[]).is_none());
        assert!(hp.take_diagnostics()[0].contains("timed out"));
        assert!(hp.take_diagnostics().is_empty());
    }

    #[test]
    fn both_error_formats_match_only_the_current_request() {
        for kind in [0x8F, 0xFF] {
            assert_eq!(reply_error(0x81B5, &[kind, 0x81, 0xB5, 2]), Some(2));
            assert_eq!(reply_error(0x81B5, &[kind, 0x81, 0xB6, 2]), None);
            assert_eq!(reply_error(0x81B5, &[kind, 0x81]), None);
        }
        assert_eq!(reply_error(0x81B5, &[0x81, 0xB5, 0, 0]), None);
    }
}
