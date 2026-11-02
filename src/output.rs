use std::borrow::Cow;

use serde::Serialize;
use thiserror::Error;

use crate::connect::ConnectReport;
use crate::device::{Tp7Device, interface_summary};
use crate::doctor::{DoctorReport, ProcessConflict};
use crate::eject::EjectReport;
use crate::ls::{LsEntry, LsReport};
use crate::pull::{PullReport, PullStatus};
use crate::push::{PushReport, PushStatus};
use crate::remote::ObjectKind;
use crate::stat::StatReport;
use crate::status::StatusReport;
use crate::tree::TreeReport;
use crate::usb_owner::UsbOwner;
use crate::write_ops::{MkdirReport, RenameReport, RmReport};

/// `println!` for human-readable output. File names, serials and other text
/// that comes from the device are escaped so they can't drive the terminal.
macro_rules! say {
    ($($arg:tt)*) => {
        println!("{}", terminal_safe(&format!($($arg)*)))
    };
}

/// Escapes control characters (C0, DEL, C1) and Unicode bidi controls so
/// device-supplied text prints literally instead of being interpreted by the
/// terminal (escape sequences, title or clipboard changes, reordered text).
/// Newlines and Unicode line separators are escaped too, so a file name can't
/// fake an extra line of output; every `say!` call prints one line.
pub fn terminal_safe(text: &str) -> Cow<'_, str> {
    if !text.chars().any(needs_escape) {
        return Cow::Borrowed(text);
    }

    let mut escaped = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        if needs_escape(character) {
            escaped.extend(character.escape_unicode());
        } else {
            escaped.push(character);
        }
    }
    Cow::Owned(escaped)
}

fn needs_escape(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

#[derive(Debug, Clone, Copy)]
pub struct LsDisplayOptions {
    pub long: bool,
    pub ids: bool,
    pub size: bool,
    pub human_readable: bool,
    pub sort: LsSort,
    pub reverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LsSort {
    Name,
    Size,
    Time,
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("USB enumeration failed: {message}")]
    UsbEnumeration { message: String },

    #[error("process inspection failed: {message}")]
    ProcessInspection { message: String },

    #[error("USB ownership inspection failed: {message}")]
    UsbOwnershipInspection { message: String },

    #[error("no TP-7 device with serial {serial} was found")]
    DeviceNotFound { serial: String },

    #[error("no TP-7 devices were found")]
    NoDevices,

    #[error("found {count} TP-7 devices; rerun with --device <serial>")]
    MultipleDevices { count: usize },

    #[error("TP-7 {serial} is in {mode} mode; no MTP-compatible interface is visible")]
    MtpNotVisible { serial: String, mode: String },

    #[error(
        "TP-7 {serial} is switched off (USB shows its bootloader mass-storage personality); turn it on and retry, or hold STOP while powering on to boot straight into MTP mode"
    )]
    PoweredOff { serial: String },

    #[error(
        "TP-7 {serial} is in {mode} mode; rerun with --auto-connect to switch into MTP mode for this command"
    )]
    AutoConnectRequired { serial: String, mode: String },

    #[error("invalid remote path {path}: {message}")]
    InvalidRemotePath { path: String, message: String },

    #[error("remote path was not found: {path}")]
    RemotePathNotFound { path: String },

    #[error("remote path is not a folder: {path}")]
    RemotePathNotDirectory { path: String },

    #[error("remote path is the storage root; use `tp7 ls /` to inspect it")]
    RemotePathIsRoot,

    #[error("remote path is a folder; rerun with --recursive: {path}")]
    RemotePathIsFolder { path: String },

    #[error("local path already exists; use --overwrite or --skip-existing: {path}")]
    LocalPathExists { path: String },

    #[error("local path is a directory: {path}")]
    LocalPathIsDirectory { path: String },

    #[error("local path is a file: {path}")]
    LocalPathIsFile { path: String },

    #[error("local path is a folder; use --recursive to upload it: {path}")]
    LocalPathIsFolder { path: String },

    #[error("remote path already exists; use --overwrite to replace it: {path}")]
    RemotePathExists { path: String },

    #[error("file operation failed for {path}: {message}")]
    FileSystem { path: String, message: String },

    #[error(
        "download verification failed for {path}: expected {expected_size} bytes, got {actual_size}"
    )]
    TransferVerification {
        path: String,
        expected_size: u64,
        actual_size: u64,
    },

    #[error("invalid arguments: {message}")]
    InvalidArguments { message: String },

    #[error("MTP operation failed: {message}")]
    Mtp { message: String },

    #[error("could not take over the TP-7 MTP interface: {message}")]
    MtpTakeover { message: String },

    #[error("MTP operation is not supported: {message}")]
    MtpUnsupported { message: String },

    #[error("MTP device is busy or owned by another process: {message}")]
    MtpExclusiveAccess { message: String },

    #[error("MIDI operation failed: {message}")]
    Midi { message: String },

    #[error("MIDI response timed out: {message}")]
    MidiTimeout { message: String },

    #[error("MIDI command 0x{command:02x} was rejected with status 0x{status:02x}: {message}")]
    MidiCommandRejected {
        command: u8,
        status: u8,
        message: String,
    },

    #[error("runtime initialization failed: {message}")]
    Runtime { message: String },

    #[error("JSON output failed: {source}")]
    Json { source: serde_json::Error },

    #[error("{command} is not implemented yet")]
    NotImplemented { command: String },
}

impl AppError {
    pub fn not_implemented(command: impl Into<String>) -> Self {
        Self::NotImplemented {
            command: command.into(),
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            AppError::NotImplemented { .. }
            | AppError::InvalidRemotePath { .. }
            | AppError::RemotePathIsRoot
            | AppError::RemotePathIsFolder { .. }
            | AppError::LocalPathExists { .. }
            | AppError::LocalPathIsDirectory { .. }
            | AppError::LocalPathIsFile { .. }
            | AppError::LocalPathIsFolder { .. }
            | AppError::RemotePathExists { .. }
            | AppError::InvalidArguments { .. } => 2,
            AppError::MtpNotVisible { .. }
            | AppError::PoweredOff { .. }
            | AppError::AutoConnectRequired { .. } => 3,
            AppError::MtpExclusiveAccess { .. } | AppError::MtpTakeover { .. } => 4,
            AppError::Midi { .. }
            | AppError::MidiTimeout { .. }
            | AppError::MidiCommandRejected { .. } => 5,
            AppError::UsbEnumeration { .. }
            | AppError::ProcessInspection { .. }
            | AppError::UsbOwnershipInspection { .. }
            | AppError::DeviceNotFound { .. }
            | AppError::NoDevices
            | AppError::MultipleDevices { .. }
            | AppError::RemotePathNotFound { .. }
            | AppError::RemotePathNotDirectory { .. }
            | AppError::FileSystem { .. }
            | AppError::TransferVerification { .. }
            | AppError::Mtp { .. }
            | AppError::MtpUnsupported { .. }
            | AppError::Runtime { .. }
            | AppError::Json { .. } => 1,
        }
    }
}

pub fn write_devices(devices: &[Tp7Device], json: bool) -> Result<(), AppError> {
    if json {
        write_json(devices)?;
        return Ok(());
    }

    if devices.is_empty() {
        say!("No TP-7 devices found.");
        return Ok(());
    }

    for device in devices {
        say!(
            "{}  {}  {}:{}  {}  {}",
            device.serial_number.as_deref().unwrap_or("<no-serial>"),
            device.mode,
            device.vendor_id_hex,
            device.product_id_hex,
            device.speed.as_deref().unwrap_or("unknown-speed"),
            device.product.as_deref().unwrap_or("TP-7")
        );
        say!("  manufacturer: {}", display_opt(&device.manufacturer));
        say!("  usb version: {}", device.usb_version);
        say!(
            "  device version: {}",
            device.device_version.as_deref().unwrap_or("unknown")
        );
        if let Some(location_id) = &device.location_id {
            say!("  macOS location id: {location_id}");
        }
        if let Some(bus_id) = &device.bus_id {
            say!("  bus: {bus_id}");
        }
        if let Some(address) = device.device_address {
            say!("  address: {address}");
        }
        say!("  interfaces: {}", interface_summary(&device.interfaces));
    }

    Ok(())
}

pub fn write_status(report: &StatusReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    say!(
        "{}  {}  {}",
        report.usb.serial_number.as_deref().unwrap_or("<no-serial>"),
        report.usb.mode,
        report.usb.product.as_deref().unwrap_or("TP-7")
    );
    say!(
        "  mtp: {} {} ({})",
        report.mtp.manufacturer,
        report.mtp.model,
        report.mtp.device_version
    );
    say!("  mtp serial: {}", report.mtp.serial_number);
    say!("  supports rename: {}", report.mtp.supports_rename);
    say!("  storages: {}", report.mtp.storage_count);

    for storage in &report.mtp.storages {
        say!(
            "  - {}: {} free of {} bytes ({})",
            storage.id,
            storage.free_space_bytes,
            storage.max_capacity_bytes,
            storage.description
        );
    }

    Ok(())
}

pub fn write_connect(report: &ConnectReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    say!(
        "{}  {} -> {}  {}",
        report.serial_number.as_deref().unwrap_or("<no-serial>"),
        report.initial_mode,
        report.final_mode,
        report.message
    );
    say!("  switched: {}", if report.switched { "yes" } else { "no" });
    say!("  mtp session: {}", report.mtp_session.message);
    if let Some(storage_count) = report.mtp_session.storage_count {
        say!("  storages: {storage_count}");
    }
    if let Some(midi_switch) = &report.midi_switch {
        say!(
            "  midi switch: command 0x{:02x}, payload {}",
            midi_switch.command,
            midi_switch.payload.join(" ")
        );
    }

    Ok(())
}

pub fn write_ls(report: &LsReport, json: bool, options: LsDisplayOptions) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    let mut entries = report.entries.clone();
    sort_ls_entries(&mut entries, options.sort);
    if options.reverse {
        entries.reverse();
    }

    for entry in &entries {
        let name = ls_display_name(entry.name.as_str(), &entry.kind);
        let size = ls_size_label(entry.size, options.human_readable);

        match (options.long, options.ids, options.size) {
            (true, true, _) => say!(
                "{:>10} {:<6} {:>12} {:<15} {}",
                entry.id,
                ls_kind_label(&entry.kind),
                size,
                entry.modified.as_deref().unwrap_or("-"),
                name
            ),
            (true, false, _) => say!(
                "{:<6} {:>12} {:<15} {}",
                ls_kind_label(&entry.kind),
                size,
                entry.modified.as_deref().unwrap_or("-"),
                name
            ),
            (false, true, true) => say!("{:>10} {size:>12} {name}", entry.id),
            (false, true, false) => say!("{:>10} {name}", entry.id),
            (false, false, true) => say!("{size:>12} {name}"),
            (false, false, false) => say!("{name}"),
        }
    }

    Ok(())
}

pub fn write_stat(report: &StatReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    say!(
        "{}  {}  {}",
        report.path,
        ls_kind_label(&report.object.kind),
        report.object.name
    );
    say!("  id: {}", report.object.id);
    say!("  parent: {}", report.object.parent_id);
    say!("  storage: {}", report.object.storage_id);
    say!("  size: {}", size_with_bytes(report.object.size));
    say!("  format: {} ({})", report.format, report.format_code);
    say!("  created: {}", report.created.as_deref().unwrap_or("-"));
    say!(
        "  modified: {}",
        report.object.modified.as_deref().unwrap_or("-")
    );

    Ok(())
}

pub fn write_tree(report: &TreeReport, json: bool, ids: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    let entry_is_requested_file =
        report.entries.len() == 1 && report.entries[0].path == report.path;
    let root_indent = if entry_is_requested_file {
        0
    } else {
        say!("{}", report.path);
        1
    };

    for entry in &report.entries {
        let indent = "  ".repeat(entry.depth + root_indent);
        let name = ls_display_name(entry.object.name.as_str(), &entry.object.kind);

        if ids {
            say!("{indent}{:>10} {name}", entry.object.id);
        } else {
            say!("{indent}{name}");
        }
    }

    Ok(())
}

pub fn write_pull(report: &PullReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    for file in &report.files {
        let status = pull_status_label(&file.status);
        say!(
            "{status} {} -> {} ({})",
            file.remote_path,
            file.local_path,
            size_with_bytes(file.size)
        );
    }

    if report.dry_run {
        let planned_bytes = report
            .files
            .iter()
            .filter(|file| file.status == PullStatus::DryRun)
            .map(|file| file.size)
            .sum();
        say!(
            "{} downloaded, {} skipped, {} would download",
            report.downloaded,
            report.skipped,
            size_with_bytes(planned_bytes)
        );
    } else {
        say!(
            "{} downloaded, {} skipped, {}",
            report.downloaded,
            report.skipped,
            size_with_bytes(report.total_bytes)
        );
    }

    Ok(())
}

pub fn write_push(report: &PushReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    for file in &report.files {
        let status = push_status_label(&file.status);
        say!(
            "{status} {} -> {} ({})",
            file.local_path,
            file.remote_path,
            size_with_bytes(file.size)
        );
    }

    if report.dry_run {
        let planned_bytes = report.files.iter().map(|file| file.size).sum();
        say!(
            "{} uploaded, {} would upload",
            report.uploaded,
            size_with_bytes(planned_bytes)
        );
    } else {
        say!(
            "{} uploaded, {}",
            report.uploaded,
            size_with_bytes(report.total_bytes)
        );
    }

    Ok(())
}

pub fn write_mkdir(report: &MkdirReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    if report.created.is_empty() {
        say!("exists {}", report.path);
    } else {
        for path in &report.created {
            say!("created {path}");
        }
    }

    Ok(())
}

pub fn write_rename(report: &RenameReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    say!("renamed {} -> {}", report.old_path, report.new_path);

    Ok(())
}

pub fn write_rm(report: &RmReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    if report.removed.is_empty() {
        say!("nothing removed");
        return Ok(());
    }

    let status = if report.dry_run {
        "would remove"
    } else {
        "removed"
    };
    for object in &report.removed {
        say!("{status} {} ({})", report.path, ls_kind_label(&object.kind));
    }

    if !report.contents.is_empty() {
        let (files, bytes) = report.content_totals();
        let noun = if files == 1 { "file" } else { "files" };
        say!(
            "{status} everything inside it: {files} {noun}, {}",
            size_with_bytes(bytes)
        );
        for entry in &report.contents {
            say!(
                "  {status} {} ({})",
                entry.path,
                ls_kind_label(&entry.object.kind)
            );
        }
    }

    Ok(())
}

pub fn write_eject(report: &EjectReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    say!(
        "{}  {} -> {}  {}",
        report.serial_number.as_deref().unwrap_or("<no-serial>"),
        report.initial_mode,
        report.final_mode,
        report.message
    );
    say!("  switched: {}", if report.switched { "yes" } else { "no" });
    say!("  closed: {}", if report.closed { "yes" } else { "no" });

    Ok(())
}

pub fn write_doctor(report: &DoctorReport, json: bool) -> Result<(), AppError> {
    if json {
        write_json(report)?;
        return Ok(());
    }

    say!("TP-7 doctor");
    for check in &report.checks {
        say!("[{}] {}: {}", check.status, check.name, check.message);
    }

    if !report.process_conflicts.is_empty() {
        println!();
        say!("Possible process conflicts:");
        for conflict in &report.process_conflicts {
            print_process_conflict(conflict);
        }
    }

    if !report.usb_owners.is_empty() {
        println!();
        say!("macOS USB owners:");
        for owner in &report.usb_owners {
            print_usb_owner(owner);
        }
    }

    if !report.devices.is_empty() {
        println!();
        say!("Detected devices:");
        for device in &report.devices {
            say!(
                "- {} ({})",
                device.serial_number.as_deref().unwrap_or("<no-serial>"),
                device.mode
            );
        }
    }

    Ok(())
}

fn write_json<T: Serialize + ?Sized>(value: &T) -> Result<(), AppError> {
    serde_json::to_writer_pretty(std::io::stdout(), value)
        .map_err(|source| AppError::Json { source })?;
    println!();
    Ok(())
}

fn display_opt(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("unknown")
}

fn ls_kind_label(kind: &ObjectKind) -> &'static str {
    match kind {
        ObjectKind::File => "file",
        ObjectKind::Folder => "folder",
    }
}

fn ls_display_name(name: &str, kind: &ObjectKind) -> String {
    match kind {
        ObjectKind::File => name.to_string(),
        ObjectKind::Folder => format!("{name}/"),
    }
}

fn sort_ls_entries(entries: &mut [LsEntry], sort: LsSort) {
    match sort {
        LsSort::Name => {}
        LsSort::Size => entries.sort_by(|left, right| {
            right
                .size
                .cmp(&left.size)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.name.cmp(&right.name))
        }),
        LsSort::Time => entries.sort_by(|left, right| {
            right
                .modified
                .cmp(&left.modified)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.name.cmp(&right.name))
        }),
    }
}

fn ls_size_label(size: u64, human_readable: bool) -> String {
    if !human_readable {
        return size.to_string();
    }

    human_size(size)
}

fn size_with_bytes(size: u64) -> String {
    if size < 1024 {
        format!("{size} bytes")
    } else {
        format!("{} ({size} bytes)", human_size(size))
    }
}

fn human_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = size as f64;
    let mut unit = 0;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{size}{}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn pull_status_label(status: &PullStatus) -> &'static str {
    match status {
        PullStatus::Downloaded => "downloaded",
        PullStatus::DryRun => "would download",
        PullStatus::SkippedExists => "skipped (exists)",
        PullStatus::SkippedTooLarge => "skipped (too large)",
    }
}

fn push_status_label(status: &PushStatus) -> &'static str {
    match status {
        PushStatus::Uploaded => "uploaded",
        PushStatus::DryRun => "would upload",
    }
}

fn print_process_conflict(conflict: &ProcessConflict) {
    say!(
        "- pid {}: {} ({})",
        conflict.pid,
        conflict.name,
        conflict.reason
    );
}

fn print_usb_owner(owner: &UsbOwner) {
    let pid = owner
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let location = match owner.interface_number {
        Some(number) => format!(
            "{} {} ({})",
            owner.scope,
            number,
            owner
                .interface_name
                .as_deref()
                .unwrap_or(owner.scope_node_name.as_str())
        ),
        None => format!("{} {}", owner.scope, owner.scope_node_name),
    };

    say!(
        "- {} pid {}: {} on {}",
        owner.kind,
        pid,
        owner.process,
        location
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_terminal_control_sequences_from_the_device() {
        assert_eq!(terminal_safe("take 1.wav"), "take 1.wav");
        assert_eq!(
            terminal_safe("a.wav\nremoved /recordings (folder)"),
            "a.wav\\u{a}removed /recordings (folder)"
        );
        assert_eq!(terminal_safe("a\u{2028}b"), "a\\u{2028}b");
        assert_eq!(
            terminal_safe("evil\u{1b}]0;pwned\u{7}.wav"),
            "evil\\u{1b}]0;pwned\\u{7}.wav"
        );
        assert_eq!(terminal_safe("\u{9b}2J"), "\\u{9b}2J");
        assert_eq!(terminal_safe("abc\u{202e}vaw.exe"), "abc\\u{202e}vaw.exe");
    }

    #[test]
    fn powered_off_error_explains_the_bootloader_personality() {
        let error = AppError::PoweredOff {
            serial: "F1RTL11C".to_string(),
        };

        assert_eq!(
            error.to_string(),
            "TP-7 F1RTL11C is switched off (USB shows its bootloader mass-storage personality); \
             turn it on and retry, or hold STOP while powering on to boot straight into MTP mode"
        );
    }

    #[test]
    fn powered_off_shares_the_mtp_not_visible_exit_code() {
        let powered_off = AppError::PoweredOff {
            serial: "F1RTL11C".to_string(),
        };
        let not_visible = AppError::MtpNotVisible {
            serial: "F1RTL11C".to_string(),
            mode: "mass-storage".to_string(),
        };

        assert_eq!(powered_off.exit_code(), not_visible.exit_code());
    }
}
