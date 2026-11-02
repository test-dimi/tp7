use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mtp_rs::{ByteRange, DEFAULT_CANCEL_TIMEOUT, ObjectHandle, ObjectInfo, Storage};
use serde::{Deserialize, Serialize};

use crate::mtp_session::{MtpOpenPolicy, block_on, map_mtp_error, open_mtp_session};
use crate::output::AppError;
use crate::remote::{
    RemoteTarget, check_folder_descent, first_storage, join_remote_path, list_object_infos,
    resolve_path,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullReport {
    pub remote_path: String,
    pub local_path: String,
    pub dry_run: bool,
    pub max_size: Option<u64>,
    pub downloaded: usize,
    pub skipped: usize,
    pub total_bytes: u64,
    pub files: Vec<PullFileReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullFileReport {
    pub remote_path: String,
    pub local_path: String,
    pub size: u64,
    pub status: PullStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PullStatus {
    Downloaded,
    DryRun,
    /// The destination file already exists and `--skip-existing` was set.
    SkippedExists,
    /// The remote file is larger than `--max-size`.
    SkippedTooLarge,
}

#[derive(Debug, Clone, Copy)]
pub struct PullOptions {
    pub recursive: bool,
    pub overwrite: bool,
    pub skip_existing: bool,
    pub max_size: Option<u64>,
    pub dry_run: bool,
    pub progress: bool,
}

pub fn run_pull(
    serial: Option<&str>,
    auto_connect: bool,
    remote_path: &str,
    local_path: Option<&str>,
    options: PullOptions,
) -> Result<PullReport, AppError> {
    validate_options(options)?;
    let policy = if auto_connect {
        MtpOpenPolicy::AutoSwitch
    } else {
        MtpOpenPolicy::RequireAutoConnectFlag
    };

    block_on(async {
        let session = open_mtp_session(serial, policy).await?;
        let result = read_pull(&session.device, remote_path, local_path, options).await;
        let release_result = session.release().await;

        match (result, release_result) {
            (Ok(report), Ok(())) => Ok(report),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    })
}

async fn read_pull(
    device: &mtp_rs::MtpDevice,
    remote_path: &str,
    local_path: Option<&str>,
    options: PullOptions,
) -> Result<PullReport, AppError> {
    let storage = first_storage(device).await?;
    let resolved = resolve_path(&storage, remote_path).await?;
    let mut report = PullReport {
        remote_path: resolved.path.clone(),
        local_path: String::new(),
        dry_run: options.dry_run,
        max_size: options.max_size,
        downloaded: 0,
        skipped: 0,
        total_bytes: 0,
        files: Vec::new(),
    };

    match resolved.target {
        RemoteTarget::Root => Err(AppError::RemotePathIsRoot),
        RemoteTarget::Object(object) if object.is_folder() => {
            if !options.recursive {
                return Err(AppError::RemotePathIsFolder {
                    path: resolved.path,
                });
            }

            let destination = folder_destination(local_path, &object.filename)?;
            report.local_path = path_to_string(&destination);
            pull_folder(
                &storage,
                object.handle,
                &resolved.path,
                &destination,
                options,
                &mut report,
            )
            .await?;
            Ok(report)
        }
        RemoteTarget::Object(object) => {
            let destination = file_destination(local_path, &object.filename)?;
            report.local_path = path_to_string(&destination);
            pull_file(
                &storage,
                object,
                &resolved.path,
                &destination,
                options,
                &mut report,
            )
            .await?;
            Ok(report)
        }
    }
}

async fn pull_folder(
    storage: &Storage,
    root_handle: ObjectHandle,
    root_remote_path: &str,
    root_local_path: &Path,
    options: PullOptions,
    report: &mut PullReport,
) -> Result<(), AppError> {
    if !options.dry_run {
        create_dir_all(root_local_path)?;
    }

    let mut visited = HashSet::from([root_handle]);
    let mut stack = list_object_infos(storage, Some(root_handle))
        .await?
        .into_iter()
        .rev()
        .map(|object| {
            (
                object,
                1,
                root_remote_path.to_string(),
                root_local_path.to_path_buf(),
            )
        })
        .collect::<Vec<_>>();

    while let Some((object, depth, remote_parent_path, local_parent_path)) = stack.pop() {
        let remote_path = join_remote_path(&remote_parent_path, &object.filename);
        let local_path = safe_child_path(&local_parent_path, &object.filename)?;

        if object.is_folder() {
            check_folder_descent(&mut visited, object.handle, depth, &remote_path)?;
            if !options.dry_run {
                create_dir_all(&local_path)?;
            }

            let children = list_object_infos(storage, Some(object.handle)).await?;
            stack.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| (child, depth + 1, remote_path.clone(), local_path.clone())),
            );
        } else {
            pull_file(storage, object, &remote_path, &local_path, options, report).await?;
        }
    }

    Ok(())
}

async fn pull_file(
    storage: &Storage,
    object: ObjectInfo,
    remote_path: &str,
    local_path: &Path,
    options: PullOptions,
    report: &mut PullReport,
) -> Result<(), AppError> {
    let status = if exceeds_max_size(object.size, options.max_size) {
        PullStatus::SkippedTooLarge
    } else {
        prepare_file_destination(local_path, options)?
    };

    match status {
        status @ (PullStatus::SkippedExists | PullStatus::SkippedTooLarge) => {
            report.skipped += 1;
            report
                .files
                .push(file_report(remote_path, local_path, object.size, status));
            Ok(())
        }
        PullStatus::DryRun => {
            report.files.push(file_report(
                remote_path,
                local_path,
                object.size,
                PullStatus::DryRun,
            ));
            Ok(())
        }
        PullStatus::Downloaded => {
            let (temp, file) = TempDownload::create(local_path)?;
            download_to_file(
                storage,
                object.handle,
                object.size,
                file,
                temp.path(),
                options,
            )
            .await?;
            // Check the finished download before it replaces anything, so a short
            // transfer can never cost the user an existing local copy.
            verify_file_size(temp.path(), object.size)?;
            rename_file(temp.path(), local_path)?;

            report.downloaded += 1;
            report.total_bytes += object.size;
            report.files.push(file_report(
                remote_path,
                local_path,
                object.size,
                PullStatus::Downloaded,
            ));
            Ok(())
        }
    }
}

async fn download_to_file(
    storage: &Storage,
    handle: ObjectHandle,
    expected_size: u64,
    mut file: File,
    local_path: &Path,
    options: PullOptions,
) -> Result<(), AppError> {
    let mut download = storage
        .download(handle, ByteRange::Full)
        .await
        .map_err(map_mtp_error)?;

    while let Some(chunk) = download.next_chunk().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = download.cancel(DEFAULT_CANCEL_TIMEOUT).await;
                finish_progress(options.progress);
                return Err(map_mtp_error(error));
            }
        };
        if let Err(error) = file.write_all(&bytes) {
            let _ = download.cancel(DEFAULT_CANCEL_TIMEOUT).await;
            finish_progress(options.progress);
            return Err(io_error(local_path, error));
        }
        write_progress(options.progress, download.bytes_received(), expected_size);
    }

    finish_progress(options.progress);
    file.flush().map_err(|error| io_error(local_path, error))?;
    file.sync_all()
        .map_err(|error| io_error(local_path, error))?;
    Ok(())
}

fn prepare_file_destination(
    local_path: &Path,
    options: PullOptions,
) -> Result<PullStatus, AppError> {
    if local_path.exists() {
        if local_path.is_dir() {
            return Err(AppError::LocalPathIsDirectory {
                path: path_to_string(local_path),
            });
        }

        if options.skip_existing {
            return Ok(PullStatus::SkippedExists);
        }

        if !options.overwrite {
            return Err(AppError::LocalPathExists {
                path: path_to_string(local_path),
            });
        }
    }

    if let Some(parent) = local_path.parent()
        && !parent.as_os_str().is_empty()
        && !options.dry_run
    {
        create_dir_all(parent)?;
    }

    if options.dry_run {
        Ok(PullStatus::DryRun)
    } else {
        Ok(PullStatus::Downloaded)
    }
}

fn file_destination(local_path: Option<&str>, remote_name: &str) -> Result<PathBuf, AppError> {
    let Some(local_path) = local_path else {
        return Ok(PathBuf::from(remote_name));
    };
    let local_path = PathBuf::from(local_path);

    if local_path.is_dir() || local_path_looks_like_directory(local_path.as_path()) {
        return safe_child_path(&local_path, remote_name);
    }

    Ok(local_path)
}

fn folder_destination(local_path: Option<&str>, remote_name: &str) -> Result<PathBuf, AppError> {
    match local_path {
        Some(local_path) => {
            let local_path = PathBuf::from(local_path);
            if local_path.exists() && !local_path.is_dir() {
                return Err(AppError::LocalPathIsFile {
                    path: path_to_string(&local_path),
                });
            }
            Ok(local_path)
        }
        None => Ok(PathBuf::from(remote_name)),
    }
}

fn safe_child_path(parent: &Path, name: &str) -> Result<PathBuf, AppError> {
    if name == "." || name == ".." || name.contains('/') {
        return Err(AppError::InvalidRemotePath {
            path: name.to_string(),
            message: "remote object names cannot contain path separators".to_string(),
        });
    }

    Ok(parent.join(name))
}

/// How many differently named temp files to try before giving up.
const TEMP_CREATE_ATTEMPTS: u32 = 16;

/// Longest slice of the destination name kept in a temp name. The suffix
/// adds about 40 bytes, and macOS file names stop at 255 bytes.
const TEMP_NAME_STEM_BYTES: usize = 160;

/// The longest prefix of `text` that fits in `max_bytes` without splitting a
/// character.
fn clip_to_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn temp_download_path(local_path: &Path, attempt: u32) -> Result<PathBuf, AppError> {
    let Some(file_name) = local_path.file_name() else {
        return Err(AppError::InvalidArguments {
            message: "local path must include a file name".to_string(),
        });
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temp_name = format!(
        ".{}.{}-{stamp}-{attempt}.tp7tmp",
        clip_to_bytes(&file_name.to_string_lossy(), TEMP_NAME_STEM_BYTES),
        std::process::id()
    );

    Ok(local_path.with_file_name(temp_name))
}

/// Owns the partial download and deletes it on drop, so a failed transfer
/// never leaves a `.tp7tmp` file next to the destination. A completed
/// download renames the file away first, which turns the cleanup into a
/// no-op. The file is created exclusively under a unique name, so it never
/// truncates or follows anything already on disk. A process killed by a
/// signal (Ctrl-C) skips `Drop` and can leave the hidden temp file behind.
struct TempDownload {
    path: PathBuf,
}

impl TempDownload {
    fn create(local_path: &Path) -> Result<(Self, File), AppError> {
        if let Some(parent) = local_path.parent()
            && !parent.as_os_str().is_empty()
        {
            create_dir_all(parent)?;
        }

        let mut attempt = 0;
        loop {
            let path = temp_download_path(local_path, attempt)?;
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((Self { path }, file)),
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists
                        && attempt + 1 < TEMP_CREATE_ATTEMPTS =>
                {
                    attempt += 1;
                }
                Err(error) => return Err(io_error(&path, error)),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDownload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn exceeds_max_size(size: u64, max_size: Option<u64>) -> bool {
    max_size.is_some_and(|max_size| size > max_size)
}

/// Parses a byte size written as plain bytes or with a `K`, `M`, or `G`
/// suffix. Suffixes are case-insensitive powers of 1024, so `512M` is
/// `536870912`.
pub fn parse_size(input: &str) -> Result<u64, AppError> {
    let text = input.trim();
    let (digits, unit) = match text.as_bytes().last() {
        Some(b'k' | b'K') => (&text[..text.len() - 1], 1024),
        Some(b'm' | b'M') => (&text[..text.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };

    digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(unit))
        .ok_or_else(|| AppError::InvalidArguments {
            message: format!(
                "{input} is not a valid size; use bytes or a K/M/G suffix, for example 512M"
            ),
        })
}

fn validate_options(options: PullOptions) -> Result<(), AppError> {
    if options.overwrite && options.skip_existing {
        return Err(AppError::InvalidArguments {
            message: "--overwrite and --skip-existing cannot be used together".to_string(),
        });
    }

    Ok(())
}

fn write_progress(enabled: bool, transferred: u64, total: u64) {
    if !enabled {
        return;
    }

    let percent = if total == 0 {
        100.0
    } else {
        transferred as f64 / total as f64 * 100.0
    };

    eprint!("\rdownloaded {transferred}/{total} bytes ({percent:.1}%)");
    let _ = io::stderr().flush();
}

fn finish_progress(enabled: bool) {
    if enabled {
        eprintln!();
    }
}

fn local_path_looks_like_directory(path: &Path) -> bool {
    path.to_string_lossy().ends_with('/')
}

fn file_report(
    remote_path: &str,
    local_path: &Path,
    size: u64,
    status: PullStatus,
) -> PullFileReport {
    PullFileReport {
        remote_path: remote_path.to_string(),
        local_path: path_to_string(local_path),
        size,
        status,
    }
}

fn create_dir_all(path: &Path) -> Result<(), AppError> {
    fs::create_dir_all(path).map_err(|error| io_error(path, error))
}

fn rename_file(from: &Path, to: &Path) -> Result<(), AppError> {
    fs::rename(from, to).map_err(|error| io_error(to, error))
}

fn verify_file_size(path: &Path, expected_size: u64) -> Result<(), AppError> {
    let actual_size = fs::metadata(path)
        .map_err(|error| io_error(path, error))?
        .len();

    if actual_size != expected_size {
        return Err(AppError::TransferVerification {
            path: path_to_string(path),
            expected_size,
            actual_size,
        });
    }

    Ok(())
}

fn io_error(path: &Path, error: std::io::Error) -> AppError {
    AppError::FileSystem {
        path: path_to_string(path),
        message: error.to_string(),
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_file_destination_to_remote_name() {
        assert_eq!(
            file_destination(None, "take.wav").unwrap(),
            PathBuf::from("take.wav")
        );
    }

    #[test]
    fn treats_trailing_slash_file_destination_as_directory() {
        assert_eq!(
            file_destination(Some("out/"), "take.wav").unwrap(),
            PathBuf::from("out/take.wav")
        );
    }

    #[test]
    fn rejects_unsafe_child_names() {
        let error = safe_child_path(Path::new("out"), "../take.wav").unwrap_err();

        assert!(matches!(error, AppError::InvalidRemotePath { .. }));
    }

    #[test]
    fn builds_unique_hidden_temp_path_next_to_destination() {
        let first = temp_download_path(Path::new("out/take.wav"), 0).unwrap();
        let second = temp_download_path(Path::new("out/take.wav"), 1).unwrap();
        let name = first.file_name().unwrap().to_string_lossy().to_string();

        assert_eq!(first.parent(), Some(Path::new("out")));
        assert!(name.starts_with(".take.wav."));
        assert!(name.ends_with(".tp7tmp"));
        assert_ne!(first, second);
    }

    #[test]
    fn keeps_temp_names_under_the_file_name_limit() {
        let long_name = format!("{}.wav", "é".repeat(120));
        let temp = temp_download_path(&Path::new("out").join(&long_name), 15).unwrap();
        let name = temp.file_name().unwrap().to_string_lossy().to_string();

        assert!(long_name.len() > 240);
        assert!(name.len() <= 255, "{} bytes", name.len());
        assert!(name.ends_with(".tp7tmp"));
        assert_eq!(clip_to_bytes("aé", 2), "a");
        assert_eq!(clip_to_bytes("short", 160), "short");
    }

    #[test]
    fn rejects_conflicting_overwrite_and_skip_options() {
        let error = validate_options(PullOptions {
            recursive: false,
            overwrite: true,
            skip_existing: true,
            max_size: None,
            dry_run: false,
            progress: false,
        })
        .unwrap_err();

        assert!(matches!(error, AppError::InvalidArguments { .. }));
    }

    #[test]
    fn parses_plain_byte_sizes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1048576").unwrap(), 1_048_576);
        assert_eq!(parse_size(" 42 ").unwrap(), 42);
    }

    #[test]
    fn parses_binary_suffixes_in_either_case() {
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1k").unwrap(), 1024);
        assert_eq!(parse_size("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size("2g").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn rejects_malformed_sizes() {
        for input in [
            "",
            "M",
            "-1",
            "1.5M",
            "10MB",
            "10 M",
            "1T",
            "18446744073709551615K",
        ] {
            let error = parse_size(input).unwrap_err();

            assert!(
                matches!(error, AppError::InvalidArguments { .. }),
                "expected {input} to be rejected"
            );
        }
    }

    #[test]
    fn skips_only_files_above_the_size_cap() {
        assert!(!exceeds_max_size(u64::MAX, None));
        assert!(!exceeds_max_size(1024, Some(1024)));
        assert!(exceeds_max_size(1025, Some(1024)));
    }

    #[test]
    fn serializes_skip_reasons_distinctly() {
        let report = PullReport {
            remote_path: "/recordings".to_string(),
            local_path: "recordings".to_string(),
            dry_run: false,
            max_size: Some(1024),
            downloaded: 0,
            skipped: 2,
            total_bytes: 0,
            files: vec![
                file_report(
                    "/recordings/a.wav",
                    Path::new("recordings/a.wav"),
                    16,
                    PullStatus::SkippedExists,
                ),
                file_report(
                    "/recordings/b.wav",
                    Path::new("recordings/b.wav"),
                    4096,
                    PullStatus::SkippedTooLarge,
                ),
            ],
        };

        let json = serde_json::to_value(&report).unwrap();

        assert_eq!(json["max_size"], 1024);
        assert_eq!(json["files"][0]["status"], "skipped-exists");
        assert_eq!(json["files"][1]["status"], "skipped-too-large");
    }

    #[test]
    fn detects_size_mismatch_on_the_temp_file() {
        let path = Path::new("target/tp7-unit-size-mismatch.wav");
        let (temp, mut file) = TempDownload::create(path).unwrap();
        file.write_all(b"abc").unwrap();
        drop(file);

        let error = verify_file_size(temp.path(), 4).unwrap_err();
        let temp_path = temp.path().to_path_buf();
        drop(temp);

        assert!(matches!(error, AppError::TransferVerification { .. }));
        assert!(!temp_path.exists());
        assert!(!path.exists());
    }

    #[test]
    fn temp_file_leaves_an_existing_destination_untouched() {
        let path = Path::new("target/tp7-unit-keep-existing.wav");
        fs::create_dir_all("target").unwrap();
        fs::write(path, b"original").unwrap();

        let (temp, mut file) = TempDownload::create(path).unwrap();
        file.write_all(b"short").unwrap();
        drop(file);
        let temp_path = temp.path().to_path_buf();
        assert_ne!(temp_path, path);
        drop(temp);

        assert_eq!(fs::read(path).unwrap(), b"original");
        assert!(!temp_path.exists());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn removes_the_temp_file_when_the_download_does_not_finish() {
        let path = Path::new("target/tp7-unit-partial-download.wav");
        let (temp, file) = TempDownload::create(path).unwrap();
        drop(file);
        let temp_path = temp.path().to_path_buf();
        assert!(temp_path.exists());

        drop(temp);

        assert!(!temp_path.exists());
    }
}
