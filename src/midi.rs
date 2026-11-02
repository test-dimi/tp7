use serde::Serialize;

use crate::device::Tp7Device;
use crate::output::AppError;

#[derive(Debug, Clone, Serialize)]
pub struct MidiSwitchReport {
    pub device_id: u8,
    pub command: u8,
    pub payload: Vec<String>,
    pub greet: Option<GreetInfo>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GreetInfo {
    pub mode: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub sw_version: Option<String>,
    pub os_version: Option<String>,
    pub sku: Option<String>,
    pub base_sku: Option<String>,
}

#[cfg(target_os = "macos")]
mod platform {
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    use coremidi::{Client, Destination, Destinations, OutputPort, PacketBuffer, Source, Sources};

    use super::{GreetInfo, MidiSwitchReport};
    use crate::device::Tp7Device;
    use crate::output::AppError;

    const TE_MIDI_ID: [u8; 3] = [0x00, 0x20, 0x76];
    const TE_SYSEX_MARKER: u8 = 0x40;
    const FLAG_REQUEST: u8 = 0x40;
    const FLAG_REQUEST_ID: u8 = 0x20;
    const STATUS_OK: u8 = 0x00;
    const STATUS_BAD_REQUEST: u8 = 0x03;
    const COMMAND_GREET: u8 = 0x01;
    const COMMAND_MODE: u8 = 0x04;
    const MODE_MTP_NEW: [u8; 2] = [0x01, 0x03];
    const MODE_MTP_LEGACY: [u8; 2] = [0x01, 0x02];

    pub fn switch_tp7_to_mtp(_device: &Tp7Device) -> Result<MidiSwitchReport, AppError> {
        let mut session = CoreMidiSession::open()?;
        let identity = session.identify(Duration::from_secs(2))?;
        let greet = session
            .send_te_request(
                identity.device_id,
                COMMAND_GREET,
                &[],
                Duration::from_secs(2),
            )
            .map(|response| parse_greet_info(&response.data))
            .ok();

        let payload = select_mtp_payload(greet.as_ref());
        let mode_result = session.send_te_request(
            identity.device_id,
            COMMAND_MODE,
            &payload,
            Duration::from_secs(2),
        );
        let payload = match mode_result {
            Ok(_) => payload,
            Err(AppError::MidiCommandRejected {
                command,
                status: STATUS_BAD_REQUEST,
                ..
            }) if command == COMMAND_MODE && payload == MODE_MTP_NEW => {
                session.send_te_request(
                    identity.device_id,
                    COMMAND_MODE,
                    &MODE_MTP_LEGACY,
                    Duration::from_secs(2),
                )?;
                MODE_MTP_LEGACY
            }
            Err(error) => return Err(error),
        };

        Ok(MidiSwitchReport {
            device_id: identity.device_id,
            command: COMMAND_MODE,
            payload: payload
                .into_iter()
                .map(|byte| format!("0x{byte:02x}"))
                .collect(),
            greet,
        })
    }

    struct CoreMidiSession {
        source: Source,
        destination: Destination,
        output_port: OutputPort,
        rx: Receiver<Vec<u8>>,
        next_request_id: u16,
        _client: Client,
        _input_port: coremidi::InputPort,
    }

    struct IdentityResponse {
        device_id: u8,
    }

    struct TeResponse {
        data: Vec<u8>,
    }

    impl CoreMidiSession {
        fn open() -> Result<Self, AppError> {
            let source = find_tp7_source()?;
            let destination = find_tp7_destination()?;
            let client = Client::new("tp7").map_err(map_os_status("create CoreMIDI client"))?;
            let output_port = client
                .output_port("tp7-output")
                .map_err(map_os_status("create CoreMIDI output port"))?;
            let (tx, rx) = mpsc::channel();
            let input_port = client
                .input_port("tp7-input", move |packet_list| {
                    for packet in packet_list.iter() {
                        let _ = tx.send(packet.data().to_vec());
                    }
                })
                .map_err(map_os_status("create CoreMIDI input port"))?;

            input_port
                .connect_source(&source)
                .map_err(map_os_status("connect CoreMIDI source"))?;

            Ok(Self {
                source,
                destination,
                output_port,
                rx,
                next_request_id: 1,
                _client: client,
                _input_port: input_port,
            })
        }

        fn identify(&mut self, timeout: Duration) -> Result<IdentityResponse, AppError> {
            self.send_raw(&[0xf0, 0x7e, 0x7f, 0x06, 0x01, 0xf7])?;
            let deadline = Instant::now() + timeout;

            loop {
                let message = self.recv_before(deadline, "waiting for MIDI identity response")?;

                if let Some(identity) = parse_identity_response(&message) {
                    return Ok(identity);
                }
            }
        }

        fn send_te_request(
            &mut self,
            device_id: u8,
            command: u8,
            payload: &[u8],
            timeout: Duration,
        ) -> Result<TeResponse, AppError> {
            let request_id = self.next_request_id;
            self.next_request_id = (self.next_request_id + 1) % 4096;
            let message = build_te_request(device_id, request_id, command, payload);
            self.send_raw(&message)?;

            let deadline = Instant::now() + timeout;
            loop {
                let message = self.recv_before(deadline, "waiting for TE SysEx response")?;

                let Some(response) = parse_te_response(&message) else {
                    continue;
                };

                if response.request_id != request_id || response.command != command {
                    continue;
                }

                if response.status == STATUS_OK {
                    return Ok(TeResponse {
                        data: response.data,
                    });
                }

                return Err(AppError::MidiCommandRejected {
                    command: response.command,
                    status: response.status,
                    message: format!("response data {}", format_hex_bytes(&response.data)),
                });
            }
        }

        fn send_raw(&self, bytes: &[u8]) -> Result<(), AppError> {
            log::debug!("MIDI TX {}", format_hex_bytes(bytes));
            let packets = PacketBuffer::new(0, bytes);
            self.output_port
                .send(&self.destination, &packets)
                .map_err(map_os_status("send CoreMIDI packet"))
        }

        fn recv_before(
            &self,
            deadline: Instant,
            reason: &'static str,
        ) -> Result<Vec<u8>, AppError> {
            let now = Instant::now();
            if now >= deadline {
                return Err(AppError::MidiTimeout {
                    message: reason.to_string(),
                });
            }

            let timeout = deadline.saturating_duration_since(now);
            let message = self
                .rx
                .recv_timeout(timeout)
                .map_err(|_| AppError::MidiTimeout {
                    message: reason.to_string(),
                })?;
            log::debug!("MIDI RX {}", format_hex_bytes(&message));
            Ok(message)
        }
    }

    impl Drop for CoreMidiSession {
        fn drop(&mut self) {
            let _ = self._input_port.disconnect_source(&self.source);
        }
    }

    struct ParsedTeResponse {
        request_id: u16,
        command: u8,
        status: u8,
        data: Vec<u8>,
    }

    fn find_tp7_source() -> Result<Source, AppError> {
        single_tp7_endpoint(
            Sources.into_iter().filter(is_tp7_endpoint).collect(),
            "sources",
            "No TP-7 CoreMIDI source endpoint was found.",
        )
    }

    fn find_tp7_destination() -> Result<Destination, AppError> {
        single_tp7_endpoint(
            Destinations.into_iter().filter(is_tp7_endpoint).collect(),
            "destinations",
            "No TP-7 CoreMIDI destination endpoint was found.",
        )
    }

    /// The mode switch must reach exactly one recorder. With two TP-7s, or a
    /// virtual MIDI port that also carries the name, refuse instead of
    /// guessing. The ambiguity message avoids the "endpoint" wording that
    /// `mtp_session` treats as a transient, retryable absence.
    fn single_tp7_endpoint<T>(
        mut matches: Vec<T>,
        kind: &str,
        missing: &str,
    ) -> Result<T, AppError> {
        match matches.len() {
            0 => Err(AppError::Midi {
                message: missing.to_string(),
            }),
            1 => Ok(matches.remove(0)),
            count => Err(AppError::Midi {
                message: format!(
                    "Found {count} MIDI {kind} named TP-7, so tp7 can't tell which recorder to switch. Disconnect the other TP-7 or rename the other MIDI port, then retry."
                ),
            }),
        }
    }

    fn is_tp7_endpoint<T>(endpoint: &T) -> bool
    where
        T: AsRef<coremidi::Object>,
    {
        endpoint
            .as_ref()
            .display_name()
            .or_else(|| endpoint.as_ref().name())
            .is_some_and(|name| name == "TP-7" || name.contains("TP-7"))
    }

    fn build_te_request(device_id: u8, request_id: u16, command: u8, payload: &[u8]) -> Vec<u8> {
        let mut message = vec![
            0xf0,
            TE_MIDI_ID[0],
            TE_MIDI_ID[1],
            TE_MIDI_ID[2],
            device_id,
            TE_SYSEX_MARKER,
            FLAG_REQUEST | FLAG_REQUEST_ID | (((request_id >> 7) & 0x1f) as u8),
            (request_id & 0x7f) as u8,
            command,
        ];

        message.extend(pack_7bit(payload));
        message.push(0xf7);
        message
    }

    fn parse_identity_response(message: &[u8]) -> Option<IdentityResponse> {
        if message.len() != 17
            || message[0] != 0xf0
            || message[1] != 0x7e
            || message[3] != 0x06
            || message[4] != 0x02
            || message[5..8] != TE_MIDI_ID
            || *message.last()? != 0xf7
        {
            return None;
        }

        Some(IdentityResponse {
            device_id: message[2],
        })
    }

    fn parse_te_response(message: &[u8]) -> Option<ParsedTeResponse> {
        if message.len() < 11
            || message[0] != 0xf0
            || message[1..4] != TE_MIDI_ID
            || message[5] != TE_SYSEX_MARKER
            || *message.last()? != 0xf7
            || message[6] & FLAG_REQUEST != 0
            || message[6] & FLAG_REQUEST_ID == 0
        {
            return None;
        }

        let request_id = (((message[6] & 0x1f) as u16) << 7) | (message[7] as u16 & 0x7f);
        let command = message[8];
        let status = message[9];
        let data = unpack_7bit(&message[10..message.len() - 1]);

        Some(ParsedTeResponse {
            request_id,
            command,
            status,
            data,
        })
    }

    fn pack_7bit(data: &[u8]) -> Vec<u8> {
        if data.is_empty() {
            return Vec::new();
        }

        let mut packed = vec![0; data.len() + data.len().div_ceil(7)];
        let mut high_index = 0;
        let mut data_index = 1;

        for (index, byte) in data.iter().enumerate() {
            let bit = index % 7;
            packed[high_index] |= (byte >> 7) << bit;
            packed[data_index] = byte & 0x7f;
            data_index += 1;

            if bit == 6 && index < data.len() - 1 {
                high_index += 8;
                data_index += 1;
            }
        }

        packed
    }

    fn unpack_7bit(data: &[u8]) -> Vec<u8> {
        let mut unpacked = Vec::new();
        let mut index = 0;

        while index < data.len() {
            let high_bits = data[index];
            index += 1;

            for bit in 0..7 {
                if index >= data.len() {
                    break;
                }

                unpacked.push((data[index] & 0x7f) | (((high_bits >> bit) & 0x01) << 7));
                index += 1;
            }
        }

        unpacked
    }

    fn parse_greet_info(data: &[u8]) -> GreetInfo {
        let text = String::from_utf8_lossy(data);
        let mut info = GreetInfo::default();

        for field in text.split(';') {
            let Some((key, value)) = field.split_once(':') else {
                continue;
            };

            match key {
                "mode" => info.mode = Some(value.to_string()),
                "product" => info.product = Some(value.to_string()),
                "serial" => info.serial = Some(value.to_string()),
                "sw_version" => info.sw_version = Some(value.to_string()),
                "os_version" => info.os_version = Some(value.to_string()),
                "sku" => info.sku = Some(value.to_string()),
                "base_sku" => info.base_sku = Some(value.to_string()),
                _ => {}
            }
        }

        info
    }

    fn select_mtp_payload(_greet: Option<&GreetInfo>) -> [u8; 2] {
        MODE_MTP_NEW
    }

    fn map_os_status(action: &'static str) -> impl FnOnce(i32) -> AppError {
        move |status| AppError::Midi {
            message: format!("{action} failed with OSStatus {status}"),
        }
    }

    fn format_hex_bytes(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn refuses_ambiguous_midi_ports() {
            assert_eq!(single_tp7_endpoint(vec![1], "sources", "none").unwrap(), 1);
            assert!(single_tp7_endpoint::<u8>(Vec::new(), "sources", "none").is_err());

            let error = single_tp7_endpoint(vec![1, 2], "sources", "none").unwrap_err();
            let AppError::Midi { message } = error else {
                panic!("expected a MIDI error");
            };
            assert!(message.starts_with("Found 2 MIDI sources"));
            assert!(!message.contains("endpoint"));
        }

        #[test]
        fn packs_and_unpacks_7bit_payload() {
            let data = [0x01, 0x03, 0x80, 0xff, b'm', b't', b'p'];

            let packed = pack_7bit(&data);

            assert_eq!(unpack_7bit(&packed), data);
        }

        #[test]
        fn builds_te_mode_request() {
            let request = build_te_request(0x19, 5, COMMAND_MODE, &MODE_MTP_NEW);

            assert_eq!(
                request,
                vec![
                    0xf0, 0x00, 0x20, 0x76, 0x19, 0x40, 0x60, 0x05, 0x04, 0x00, 0x01, 0x03, 0xf7
                ]
            );
        }

        #[test]
        fn parses_te_greet_response() {
            let response = [
                0xf0, 0x00, 0x20, 0x76, 0x19, 0x40, 0x20, 0x01, 0x01, 0x00, 0x00, b'm', b'o', b'd',
                b'e', b':', b'n', b'o', 0x00, b'r', b'm', b'a', b'l', b';', b'p', b'r', 0x00, b'o',
                b'd', b'u', b'c', b't', b':', b'T', 0x00, b'P', b'-', b'7', b';', 0xf7,
            ];

            let parsed = parse_te_response(&response).expect("response parses");
            let greet = parse_greet_info(&parsed.data);

            assert_eq!(parsed.request_id, 1);
            assert_eq!(parsed.command, COMMAND_GREET);
            assert_eq!(parsed.status, STATUS_OK);
            assert_eq!(greet.mode.as_deref(), Some("normal"));
            assert_eq!(greet.product.as_deref(), Some("TP-7"));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::MidiSwitchReport;
    use crate::device::Tp7Device;
    use crate::output::AppError;

    pub fn switch_tp7_to_mtp(_device: &Tp7Device) -> Result<MidiSwitchReport, AppError> {
        Err(AppError::Midi {
            message: "Automatic TP-7 MTP switching currently uses CoreMIDI and is only implemented on macOS.".to_string(),
        })
    }
}

pub fn switch_tp7_to_mtp(device: &Tp7Device) -> Result<MidiSwitchReport, AppError> {
    platform::switch_tp7_to_mtp(device)
}
