use std::future::Future;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use mtp_rs::MtpDevice;

use crate::device::{
    TP7_MTP_PRODUCT_ID, TP7_VENDOR_ID, Tp7Device, UsbMode, list_tp7_devices, select_one_device,
};
use crate::midi::{MidiSwitchReport, switch_tp7_to_mtp};
use crate::output::AppError;
use crate::usb_owner::{inspect_tp7_usb_owners, ptpcamerad_exclusive_owner_pids};

pub const KNOWN_TP7: &[(u16, u16)] = &[(TP7_VENDOR_ID, TP7_MTP_PRODUCT_ID)];
const TP7_SESSION_ID: u32 = 0xBAAA_AAAD;

/// How often the USB device list is re-read while waiting for the TP-7 to
/// reach a given state.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long `AutoSwitch` waits for a switched-off TP-7 to be turned on. The
/// same window the MIDI switch and MTP re-enumeration waits use.
const POWER_ON_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Copy)]
pub enum MtpOpenPolicy {
    MtpOnly,
    AutoSwitch,
    RequireAutoConnectFlag,
}

#[derive(Debug, Clone)]
pub struct PreparedMtpDevice {
    pub initial_usb: Tp7Device,
    pub usb: Tp7Device,
    pub switched: bool,
    pub midi_switch: Option<MidiSwitchReport>,
}

pub struct MtpSession {
    pub prepared: PreparedMtpDevice,
    pub device: MtpDevice,
}

impl MtpSession {
    /// Leave the device-side session open and let CLI process exit reclaim USB handles.
    pub async fn release(self) -> Result<(), AppError> {
        std::mem::forget(self);
        Ok(())
    }

    /// Explicitly close the device-side session for `tp7 eject`.
    pub async fn eject(self) -> Result<(), AppError> {
        self.device.close().await.map_err(map_mtp_error)
    }
}

/// Release a validation connection without sending the TP-7 `CloseSession`.
pub fn release_mtp_device(device: MtpDevice) {
    std::mem::forget(device);
}

pub fn block_on<T, F>(future: F) -> Result<T, AppError>
where
    F: Future<Output = Result<T, AppError>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AppError::Runtime {
            message: error.to_string(),
        })?;

    runtime.block_on(future)
}

pub fn prepare_mtp_device(
    serial: Option<&str>,
    policy: MtpOpenPolicy,
) -> Result<PreparedMtpDevice, AppError> {
    let initial_usb = wait_for_tp7_device(serial, Duration::from_secs(4))?;

    // A switched-off TP-7 still enumerates: the te-boot bootloader exposes a
    // bulk-only mass-storage personality with no MIDI, and no host request
    // moves it out of that state. Only the power switch does.
    let initial_usb = if initial_usb.mode == UsbMode::MassStorage {
        wait_for_power_on(&initial_usb, serial, policy)?
    } else {
        initial_usb
    };

    if is_mtp_visible(&initial_usb) {
        return Ok(PreparedMtpDevice {
            initial_usb: initial_usb.clone(),
            usb: initial_usb,
            switched: false,
            midi_switch: None,
        });
    }

    if initial_usb.mode != UsbMode::AudioMidi {
        return Err(AppError::MtpNotVisible {
            serial: serial_for_error(&initial_usb),
            mode: initial_usb.mode.to_string(),
        });
    }

    match policy {
        MtpOpenPolicy::AutoSwitch => {}
        MtpOpenPolicy::MtpOnly => {
            return Err(AppError::MtpNotVisible {
                serial: serial_for_error(&initial_usb),
                mode: initial_usb.mode.to_string(),
            });
        }
        MtpOpenPolicy::RequireAutoConnectFlag => {
            return Err(AppError::AutoConnectRequired {
                serial: serial_for_error(&initial_usb),
                mode: initial_usb.mode.to_string(),
            });
        }
    }

    let midi_switch = switch_tp7_to_mtp_with_retry(&initial_usb, Duration::from_secs(12))?;
    let usb = wait_for_mtp(&initial_usb, serial, Duration::from_secs(12))?;

    Ok(PreparedMtpDevice {
        initial_usb,
        usb,
        switched: true,
        midi_switch: Some(midi_switch),
    })
}

fn wait_for_tp7_device(serial: Option<&str>, timeout: Duration) -> Result<Tp7Device, AppError> {
    let deadline = Instant::now() + timeout;

    loop {
        match select_one_device(list_tp7_devices()?, serial) {
            Ok(device) => return Ok(device),
            Err(error) if is_transient_device_absence(&error) && Instant::now() < deadline => {
                thread::sleep(DEVICE_POLL_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Gives the user a window to flip the power switch on a TP-7 that is sitting
/// in its bootloader mass-storage personality, then picks the device back up
/// in whichever working personality it comes back as. Only `AutoSwitch` waits;
/// the other policies report the state and stop.
fn wait_for_power_on(
    device: &Tp7Device,
    serial: Option<&str>,
    policy: MtpOpenPolicy,
) -> Result<Tp7Device, AppError> {
    let powered_off = || AppError::PoweredOff {
        serial: serial_for_error(device),
    };

    if !matches!(policy, MtpOpenPolicy::AutoSwitch) {
        return Err(powered_off());
    }

    eprintln!("{}", powered_off());
    eprintln!(
        "waiting up to {} s for the TP-7 to come back",
        POWER_ON_TIMEOUT.as_secs()
    );

    let serial = serial.or(device.serial_number.as_deref());

    wait_for_device_state(serial, POWER_ON_TIMEOUT, is_powered_on)?.ok_or_else(powered_off)
}

pub async fn open_mtp_session(
    serial: Option<&str>,
    policy: MtpOpenPolicy,
) -> Result<MtpSession, AppError> {
    let prepared = prepare_mtp_device(serial, policy)?;
    open_prepared_mtp_session(prepared).await
}

pub async fn open_mtp_session_with_takeover(
    serial: Option<&str>,
    policy: MtpOpenPolicy,
    take_over: bool,
) -> Result<MtpSession, AppError> {
    let prepared = prepare_mtp_device(serial, policy)?;
    let first_open = open_prepared_mtp_session(prepared.clone()).await;

    match first_open {
        Err(error @ AppError::MtpExclusiveAccess { .. }) if take_over => {
            if !take_over_from_ptpcamerad()? {
                return Err(error);
            }

            retry_open_prepared_mtp_session(prepared, Duration::from_secs(2)).await
        }
        result => result,
    }
}

async fn retry_open_prepared_mtp_session(
    prepared: PreparedMtpDevice,
    timeout: Duration,
) -> Result<MtpSession, AppError> {
    let deadline = Instant::now() + timeout;

    loop {
        match open_prepared_mtp_session(prepared.clone()).await {
            Err(AppError::MtpExclusiveAccess { .. }) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(100));
            }
            result => return result,
        }
    }
}

fn take_over_from_ptpcamerad() -> Result<bool, AppError> {
    let owners = inspect_tp7_usb_owners()?;
    let pids = ptpcamerad_exclusive_owner_pids(&owners);

    if pids.is_empty() {
        return Ok(false);
    }

    for pid in pids {
        log::warn!("terminating ptpcamerad process {pid} to claim the TP-7 MTP interface");
        let status = Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .map_err(|error| AppError::MtpTakeover {
                message: format!("could not terminate ptpcamerad process {pid}: {error}"),
            })?;

        if !status.success() {
            return Err(AppError::MtpTakeover {
                message: format!("could not terminate ptpcamerad process {pid}: {status}"),
            });
        }
    }

    Ok(true)
}

pub async fn open_prepared_mtp_session(
    prepared: PreparedMtpDevice,
) -> Result<MtpSession, AppError> {
    let device = open_mtp_device(prepared.usb.serial_number.as_deref()).await?;

    Ok(MtpSession { prepared, device })
}

pub async fn open_mtp_device(serial: Option<&str>) -> Result<MtpDevice, AppError> {
    let device = match serial {
        Some(serial) => {
            MtpDevice::builder()
                .known_devices(KNOWN_TP7)
                .reuse_existing_session(TP7_SESSION_ID)
                .open_by_serial(serial)
                .await
        }
        None => {
            MtpDevice::builder()
                .known_devices(KNOWN_TP7)
                .reuse_existing_session(TP7_SESSION_ID)
                .open_first()
                .await
        }
    }
    .map_err(map_mtp_error)?;

    initialize_tp7_session(&device).await?;

    Ok(device)
}

async fn initialize_tp7_session(device: &MtpDevice) -> Result<(), AppError> {
    device.storages().await.map_err(map_mtp_error)?;

    Ok(())
}

pub fn map_mtp_error(error: mtp_rs::Error) -> AppError {
    if error.is_exclusive_access() {
        return AppError::MtpExclusiveAccess {
            message: error.to_string(),
        };
    }

    AppError::Mtp {
        message: error.to_string(),
    }
}

fn switch_tp7_to_mtp_with_retry(
    device: &Tp7Device,
    timeout: Duration,
) -> Result<MidiSwitchReport, AppError> {
    let deadline = Instant::now() + timeout;

    loop {
        match switch_tp7_to_mtp(device) {
            Ok(report) => return Ok(report),
            Err(error) if is_transient_midi_error(&error) && Instant::now() < deadline => {
                thread::sleep(DEVICE_POLL_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

fn wait_for_mtp(
    initial_device: &Tp7Device,
    serial: Option<&str>,
    timeout: Duration,
) -> Result<Tp7Device, AppError> {
    let serial = serial.or(initial_device.serial_number.as_deref());

    wait_for_device_state(serial, timeout, is_mtp_visible)?.ok_or_else(|| AppError::MtpNotVisible {
        serial: serial_for_error(initial_device),
        mode: initial_device.mode.to_string(),
    })
}

/// Re-reads the USB device list until a TP-7 matching `serial` satisfies
/// `is_ready`. Returns `Ok(None)` when the timeout expires first.
fn wait_for_device_state(
    serial: Option<&str>,
    timeout: Duration,
    is_ready: fn(&Tp7Device) -> bool,
) -> Result<Option<Tp7Device>, AppError> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let devices = match serial {
            Some(serial) => list_tp7_devices()?
                .into_iter()
                .filter(|device| device.serial_number.as_deref() == Some(serial))
                .collect::<Vec<_>>(),
            None => list_tp7_devices()?,
        };

        if let Some(device) = devices.into_iter().find(&is_ready) {
            return Ok(Some(device));
        }

        thread::sleep(DEVICE_POLL_INTERVAL);
    }

    Ok(None)
}

fn is_mtp_visible(device: &Tp7Device) -> bool {
    matches!(device.mode, UsbMode::Mtp | UsbMode::Mixed)
}

/// A TP-7 that is switched on: either the audio/MIDI personality, which takes
/// the SysEx mode switch, or MTP, which needs no switch at all.
fn is_powered_on(device: &Tp7Device) -> bool {
    device.mode == UsbMode::AudioMidi || is_mtp_visible(device)
}

fn is_transient_device_absence(error: &AppError) -> bool {
    matches!(error, AppError::NoDevices | AppError::DeviceNotFound { .. })
}

/// MIDI failures that clear up on their own shortly after the TP-7
/// enumerates: CoreMIDI has not published its endpoints yet, or the device
/// is not answering SysEx yet (seen right after power-on and after replug).
fn is_transient_midi_error(error: &AppError) -> bool {
    match error {
        AppError::Midi { message } => {
            message.contains("CoreMIDI source endpoint")
                || message.contains("CoreMIDI destination endpoint")
        }
        AppError::MidiTimeout { .. } => true,
        _ => false,
    }
}

fn serial_for_error(device: &Tp7Device) -> String {
    device
        .serial_number
        .clone()
        .unwrap_or_else(|| "<no-serial>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device_in(mode: UsbMode) -> Tp7Device {
        Tp7Device {
            vendor_id: TP7_VENDOR_ID,
            product_id: TP7_MTP_PRODUCT_ID,
            vendor_id_hex: "0x2367".to_string(),
            product_id_hex: "0x0019".to_string(),
            manufacturer: Some("teenage engineering".to_string()),
            product: Some("TP-7".to_string()),
            serial_number: Some("F1RTL11C".to_string()),
            mode,
            speed: Some("high".to_string()),
            usb_version: "2.0.0".to_string(),
            device_version: Some("2.5.7".to_string()),
            class: 0,
            subclass: 0,
            protocol: 0,
            bus_id: Some("01".to_string()),
            device_address: Some(1),
            port_chain: vec![1],
            location_id: Some("0x01100000".to_string()),
            registry_entry_id: Some("0x00000001000b6528".to_string()),
            interfaces: vec![],
        }
    }

    #[test]
    fn treats_working_personalities_as_powered_on() {
        assert!(is_powered_on(&device_in(UsbMode::AudioMidi)));
        assert!(is_powered_on(&device_in(UsbMode::Mtp)));
        assert!(is_powered_on(&device_in(UsbMode::Mixed)));
    }

    #[test]
    fn treats_the_bootloader_personality_as_switched_off() {
        assert!(!is_powered_on(&device_in(UsbMode::MassStorage)));
        assert!(!is_powered_on(&device_in(UsbMode::Unknown)));
    }

    #[test]
    fn only_auto_switch_waits_for_the_power_switch() {
        let device = device_in(UsbMode::MassStorage);

        for policy in [
            MtpOpenPolicy::MtpOnly,
            MtpOpenPolicy::RequireAutoConnectFlag,
        ] {
            let error = wait_for_power_on(&device, None, policy).unwrap_err();

            assert!(matches!(error, AppError::PoweredOff { serial } if serial == "F1RTL11C"));
        }
    }

    #[test]
    fn retries_midi_failures_that_follow_enumeration() {
        let no_source = AppError::Midi {
            message: "No TP-7 CoreMIDI source endpoint was found.".to_string(),
        };
        let silent = AppError::MidiTimeout {
            message: "waiting for MIDI identity response".to_string(),
        };
        let rejected = AppError::Midi {
            message: "unexpected SysEx reply".to_string(),
        };

        assert!(is_transient_midi_error(&no_source));
        assert!(is_transient_midi_error(&silent));
        assert!(!is_transient_midi_error(&rejected));
        assert!(!is_transient_midi_error(&AppError::NoDevices));
    }
}
