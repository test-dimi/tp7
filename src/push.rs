use std::fs::File;
use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::Stream;
use mtp_rs::{NewObjectInfo, ObjectHandle, ObjectInfo, Storage};
use serde::{Deserialize, Serialize};

use crate::mtp_session::{MtpOpenPolicy, block_on, map_mtp_error, open_mtp_session};
use crate::output::AppError;
use crate::remote::{
    RemoteTarget, first_storage, format_path, join_remote_path, path_components, resolve_path,
};

const UPLOAD_CHUNK_SIZE: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushReport {
    pub local_path: String,
    pub remote_path: String,
    pub dry_run: bool,
    pub uploaded: usize,
    pub total_bytes: u64,
    pub files: Vec<PushFileReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushFileReport {
    pub local_path: String,
    pub remote_path: String,
    pub size: u64,
    pub status: PushStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PushStatus {
    Uploaded,
    DryRun,
}

#[derive(Debug, Clone, Copy)]
pub struct PushOptions {
    pub recursive: bool,
    pub overwrite: bool,
    pub dry_run: bool,
    pub progress: bool,
}

struct RemoteDestination {
    parent: Option<ObjectHandle>,
    path: String,
    name: String,
    existing: Option<ObjectInfo>,
}

struct PushPlanItem {
    local_path: PathBuf,
    destination: RemoteDestination,
    size: u64,
}

pub fn run_push(
    serial: Option<&str>,
    auto_connect: bool,
    local_path: &str,
    remote_path: &str,
    options: PushOptions,
) -> Result<PushReport, AppError> {
    let local_path = PathBuf::from(local_path);
    validate_local_source(&local_path, options)?;
    let policy = if auto_connect {
        MtpOpenPolicy::AutoSwitch
    } else {
        MtpOpenPolicy::RequireAutoConnectFlag
    };

    block_on(async {
        let session = open_mtp_session(serial, policy).await?;
        let result = write_push(&session.device, &local_path, remote_path, options).await;
        let release_result = session.release().await;

        match (result, release_result) {
            (Ok(report), Ok(())) => Ok(report),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    })
}

async fn write_push(
    device: &mtp_rs::MtpDevice,
    local_path: &Path,
    remote_path: &str,
    options: PushOptions,
) -> Result<PushReport, AppError> {
    let storage = first_storage(device).await?;
    let rename_supported = device.supports_rename();
    if local_metadata(local_path)?.is_dir() {
        return write_push_directory(&storage, local_path, remote_path, options, rename_supported)
            .await;
    }

    let destination = resolve_file_destination(&storage, local_path, remote_path).await?;
    let size = local_metadata(local_path)?.len();
    let mut report = PushReport {
        local_path: path_to_string(local_path),
        remote_path: destination.path.clone(),
        dry_run: options.dry_run,
        uploaded: 0,
        total_bytes: 0,
        files: Vec::new(),
    };

    validate_destination_for_write(&destination, options.overwrite)?;

    if options.dry_run {
        report.files.push(file_report(
            local_path,
            &destination.path,
            size,
            PushStatus::DryRun,
        ));
        return Ok(report);
    }

    put_file(
        &storage,
        local_path,
        &destination,
        size,
        options,
        rename_supported,
    )
    .await?;

    report.uploaded = 1;
    report.total_bytes = size;
    report.files.push(file_report(
        local_path,
        &destination.path,
        size,
        PushStatus::Uploaded,
    ));
    Ok(report)
}

async fn write_push_directory(
    storage: &Storage,
    local_path: &Path,
    remote_path: &str,
    options: PushOptions,
    rename_supported: bool,
) -> Result<PushReport, AppError> {
    if !options.recursive {
        return Err(AppError::LocalPathIsFolder {
            path: path_to_string(local_path),
        });
    }

    let root = resolve_remote_folder(storage, remote_path).await?;
    let mut report = PushReport {
        local_path: path_to_string(local_path),
        remote_path: root.path.clone(),
        dry_run: options.dry_run,
        uploaded: 0,
        total_bytes: 0,
        files: Vec::new(),
    };
    let plan = plan_directory_push(
        storage,
        local_path,
        root.parent,
        &root.path,
        options.overwrite,
    )
    .await?;

    for item in plan {
        if options.dry_run {
            report.files.push(file_report(
                &item.local_path,
                &item.destination.path,
                item.size,
                PushStatus::DryRun,
            ));
        } else {
            put_file(
                storage,
                &item.local_path,
                &item.destination,
                item.size,
                options,
                rename_supported,
            )
            .await?;
            report.uploaded += 1;
            report.total_bytes += item.size;
            report.files.push(file_report(
                &item.local_path,
                &item.destination.path,
                item.size,
                PushStatus::Uploaded,
            ));
        }
    }

    Ok(report)
}

async fn plan_directory_push(
    storage: &Storage,
    local_root: &Path,
    remote_parent: Option<ObjectHandle>,
    remote_parent_path: &str,
    overwrite: bool,
) -> Result<Vec<PushPlanItem>, AppError> {
    let mut plan = Vec::new();
    let mut stack = vec![(
        local_root.to_path_buf(),
        remote_parent,
        remote_parent_path.to_string(),
    )];

    while let Some((local_dir, remote_parent, remote_parent_path)) = stack.pop() {
        for entry in sorted_directory_entries(&local_dir)? {
            let metadata = local_metadata(&entry)?;
            let name = local_file_name(&entry)?;
            let remote_child_path = join_remote_path(&remote_parent_path, &name);

            if metadata.is_dir() {
                let Some(remote_child) = find_child(storage, remote_parent, &name).await? else {
                    return Err(AppError::MtpUnsupported {
                        message: format!(
                            "remote folder creation is not available; create {remote_child_path} before pushing this directory"
                        ),
                    });
                };

                if !remote_child.is_folder() {
                    return Err(AppError::RemotePathNotDirectory {
                        path: remote_child_path,
                    });
                }

                stack.push((entry, Some(remote_child.handle), remote_child_path));
                continue;
            }

            if !metadata.is_file() {
                return Err(AppError::FileSystem {
                    path: path_to_string(&entry),
                    message: "unsupported local file type".to_string(),
                });
            }

            let destination = RemoteDestination {
                parent: remote_parent,
                path: remote_child_path,
                name,
                existing: find_child(storage, remote_parent, local_file_name(&entry)?.as_str())
                    .await?,
            };

            validate_destination_for_write(&destination, overwrite)?;
            plan.push(PushPlanItem {
                local_path: entry,
                destination,
                size: metadata.len(),
            });
        }
    }

    Ok(plan)
}

async fn put_file(
    storage: &Storage,
    local_path: &Path,
    destination: &RemoteDestination,
    size: u64,
    options: PushOptions,
    rename_supported: bool,
) -> Result<(), AppError> {
    if let Some(existing) = &destination.existing {
        upload_replacement_file(storage, local_path, destination, existing, size, options).await?;
    } else if rename_supported {
        upload_new_file(storage, local_path, destination, size, options).await?;
    } else {
        // Without rename there is no way to move a finished temp file into
        // place, so upload under the real name; a failure still removes the
        // partial file.
        upload_file(
            storage,
            local_path,
            destination,
            &destination.name,
            size,
            options,
        )
        .await?;
    }

    Ok(())
}

/// Uploads under a temporary name and renames into place only once every
/// byte has arrived, so an interrupted push (Ctrl-C, a pulled cable) can
/// never leave a truncated file under the final name.
async fn upload_new_file(
    storage: &Storage,
    local_path: &Path,
    destination: &RemoteDestination,
    size: u64,
    options: PushOptions,
) -> Result<(), AppError> {
    let temporary = fresh_temp_destination(destination, "upload");
    let handle = upload_file(
        storage,
        local_path,
        &temporary,
        &destination.name,
        size,
        options,
    )
    .await?;

    if let Err(error) = storage
        .rename(handle, &destination.name)
        .await
        .map_err(map_mtp_error)
    {
        cleanup_uploaded_temp(storage, handle).await;
        return Err(error);
    }

    Ok(())
}

async fn upload_replacement_file(
    storage: &Storage,
    local_path: &Path,
    destination: &RemoteDestination,
    existing: &ObjectInfo,
    size: u64,
    options: PushOptions,
) -> Result<(), AppError> {
    let temporary = fresh_temp_destination(destination, "upload");
    let temporary_handle = upload_file(
        storage,
        local_path,
        &temporary,
        &destination.name,
        size,
        options,
    )
    .await?;
    let backup = unique_temp_destination(storage, destination, "backup").await?;

    if let Err(error) = storage
        .rename(existing.handle, &backup.name)
        .await
        .map_err(map_mtp_error)
    {
        cleanup_uploaded_temp(storage, temporary_handle).await;
        return Err(error);
    }

    if let Err(error) = storage
        .rename(temporary_handle, &destination.name)
        .await
        .map_err(map_mtp_error)
    {
        cleanup_uploaded_temp(storage, temporary_handle).await;
        if let Err(restore_error) = storage.rename(existing.handle, &destination.name).await {
            return Err(AppError::Mtp {
                message: format!(
                    "could not finish replacing {} ({error}); the original is kept as {} because renaming it back also failed: {restore_error}",
                    destination.path, backup.path
                ),
            });
        }
        return Err(error);
    }

    if let Err(error) = storage.delete(existing.handle).await {
        return Err(AppError::Mtp {
            message: format!(
                "replacement uploaded, but cleanup of temporary backup {} failed: {error}",
                backup.path
            ),
        });
    }

    Ok(())
}

/// Uploads `local_path` as `destination`. The MTP object format (WAV, MP3,
/// ...) comes from `final_name`, the name the file ends up with, so a file
/// staged under a `.tmp` name still arrives typed as audio.
async fn upload_file(
    storage: &Storage,
    local_path: &Path,
    destination: &RemoteDestination,
    final_name: &str,
    size: u64,
    options: PushOptions,
) -> Result<ObjectHandle, AppError> {
    let stream = FileChunkStream::open(local_path)?;
    let info = staged_object_info(&destination.name, final_name, size);
    let result = storage
        .upload_with_progress(destination.parent, info, stream, |progress| {
            write_progress(
                options.progress,
                progress.bytes_transferred,
                progress.total_bytes.unwrap_or(size),
            );
            ControlFlow::Continue(())
        })
        .await;

    finish_progress(options.progress);
    match result {
        Ok(handle) => Ok(handle),
        Err(error) => Err(discard_partial_upload(storage, destination, error).await),
    }
}

/// A failed data phase can leave a truncated object on the TP-7 under the
/// name being uploaded. Delete it, so a failed push never looks like a
/// finished file, and say so in the error when the delete fails too.
async fn discard_partial_upload(
    storage: &Storage,
    destination: &RemoteDestination,
    error: mtp_rs::UploadError,
) -> AppError {
    let Some(partial) = error.partial else {
        return map_mtp_error(error.source);
    };

    match storage.delete(partial).await {
        Ok(()) => map_mtp_error(error.source),
        Err(delete_error) => AppError::Mtp {
            message: format!(
                "upload of {} failed ({}), and the partial file left on the TP-7 could not be removed ({delete_error}); delete it with `tp7 -a rm {}`",
                destination.path, error.source, destination.path
            ),
        },
    }
}

async fn resolve_file_destination(
    storage: &Storage,
    local_path: &Path,
    remote_path: &str,
) -> Result<RemoteDestination, AppError> {
    let local_name = local_file_name(local_path)?;
    let remote_path = remote_path.trim();

    if remote_path.is_empty() || remote_path == "/" {
        return destination_in_folder(storage, None, "/", local_name).await;
    }

    if remote_path.ends_with('/') {
        let resolved = resolve_path(storage, remote_path).await?;
        return match resolved.target {
            RemoteTarget::Root => destination_in_folder(storage, None, "/", local_name).await,
            RemoteTarget::Object(folder) if folder.is_folder() => {
                destination_in_folder(storage, Some(folder.handle), &resolved.path, local_name)
                    .await
            }
            RemoteTarget::Object(_) => Err(AppError::RemotePathNotDirectory {
                path: resolved.path,
            }),
        };
    }

    match resolve_path(storage, remote_path).await {
        Ok(resolved) => match resolved.target {
            RemoteTarget::Root => destination_in_folder(storage, None, "/", local_name).await,
            RemoteTarget::Object(folder) if folder.is_folder() => {
                destination_in_folder(storage, Some(folder.handle), &resolved.path, local_name)
                    .await
            }
            RemoteTarget::Object(object) => Ok(RemoteDestination {
                parent: parent_handle(object.parent),
                path: resolved.path,
                name: object.filename.clone(),
                existing: Some(object),
            }),
        },
        Err(AppError::RemotePathNotFound { .. }) => {
            let components = path_components(remote_path)?;
            let Some(name) = components.last().cloned() else {
                return Err(AppError::RemotePathIsRoot);
            };
            let parent_components = &components[..components.len() - 1];
            let parent_path = format_path(parent_components);
            let parent = resolve_path(storage, &parent_path).await?;
            match parent.target {
                RemoteTarget::Root => Ok(RemoteDestination {
                    parent: None,
                    path: join_remote_path("/", &name),
                    name,
                    existing: None,
                }),
                RemoteTarget::Object(parent) if parent.is_folder() => Ok(RemoteDestination {
                    parent: Some(parent.handle),
                    path: join_remote_path(&parent_path, &name),
                    name,
                    existing: None,
                }),
                RemoteTarget::Object(_) => {
                    Err(AppError::RemotePathNotDirectory { path: parent_path })
                }
            }
        }
        Err(error) => Err(error),
    }
}

async fn destination_in_folder(
    storage: &Storage,
    parent: Option<ObjectHandle>,
    parent_path: &str,
    name: String,
) -> Result<RemoteDestination, AppError> {
    let path = join_remote_path(parent_path, &name);
    let existing = find_child(storage, parent, &name).await?;

    Ok(RemoteDestination {
        parent,
        path,
        name,
        existing,
    })
}

struct RemoteFolder {
    parent: Option<ObjectHandle>,
    path: String,
}

async fn resolve_remote_folder(
    storage: &Storage,
    remote_path: &str,
) -> Result<RemoteFolder, AppError> {
    let resolved = resolve_path(storage, remote_path).await?;
    match resolved.target {
        RemoteTarget::Root => Ok(RemoteFolder {
            parent: None,
            path: resolved.path,
        }),
        RemoteTarget::Object(folder) if folder.is_folder() => Ok(RemoteFolder {
            parent: Some(folder.handle),
            path: resolved.path,
        }),
        RemoteTarget::Object(_) => Err(AppError::RemotePathNotDirectory {
            path: resolved.path,
        }),
    }
}

fn validate_local_source(local_path: &Path, options: PushOptions) -> Result<(), AppError> {
    let metadata = local_metadata(local_path)?;

    if metadata.is_dir() && !options.recursive {
        return Err(AppError::LocalPathIsFolder {
            path: path_to_string(local_path),
        });
    }

    if !metadata.is_dir() && !metadata.is_file() {
        return Err(AppError::FileSystem {
            path: path_to_string(local_path),
            message: "unsupported local file type".to_string(),
        });
    }

    Ok(())
}

fn local_metadata(path: &Path) -> Result<std::fs::Metadata, AppError> {
    std::fs::metadata(path).map_err(|error| AppError::FileSystem {
        path: path_to_string(path),
        message: error.to_string(),
    })
}

fn local_file_name(path: &Path) -> Result<String, AppError> {
    let Some(file_name) = path.file_name() else {
        return Err(AppError::InvalidArguments {
            message: "local path must include a file name".to_string(),
        });
    };

    Ok(file_name.to_string_lossy().to_string())
}

fn parent_handle(parent: ObjectHandle) -> Option<ObjectHandle> {
    if parent.0 == 0 { None } else { Some(parent) }
}

fn validate_destination_for_write(
    destination: &RemoteDestination,
    overwrite: bool,
) -> Result<(), AppError> {
    if let Some(existing) = &destination.existing {
        if existing.is_folder() {
            return Err(AppError::RemotePathIsFolder {
                path: destination.path.clone(),
            });
        }

        if !overwrite {
            return Err(AppError::RemotePathExists {
                path: destination.path.clone(),
            });
        }
    }

    Ok(())
}

/// Object info for a file uploaded as `upload_name` that will be renamed to
/// `final_name`: the format is chosen from the final name's extension.
fn staged_object_info(upload_name: &str, final_name: &str, size: u64) -> NewObjectInfo {
    let mut info = NewObjectInfo::file(final_name, size);
    info.filename = upload_name.to_string();
    info
}

/// A temporary destination next to `destination` that needs no folder
/// listing: the name carries the process id, a millisecond stamp and the
/// label, so it can't collide with a user's file in practice, and it only
/// exists until the finished upload is renamed.
fn fresh_temp_destination(destination: &RemoteDestination, label: &str) -> RemoteDestination {
    let parent_path = parent_remote_path(&destination.path);
    let name = replacement_temp_name(&destination.name, label, 0);

    RemoteDestination {
        parent: destination.parent,
        path: join_remote_path(&parent_path, &name),
        name,
        existing: None,
    }
}

async fn unique_temp_destination(
    storage: &Storage,
    destination: &RemoteDestination,
    label: &str,
) -> Result<RemoteDestination, AppError> {
    let parent_path = parent_remote_path(&destination.path);

    for attempt in 0..32 {
        let name = replacement_temp_name(&destination.name, label, attempt);
        if find_child(storage, destination.parent, &name)
            .await?
            .is_none()
        {
            return Ok(RemoteDestination {
                parent: destination.parent,
                path: join_remote_path(&parent_path, &name),
                name,
                existing: None,
            });
        }
    }

    Err(AppError::Mtp {
        message: format!("could not allocate a temporary upload name under {parent_path}"),
    })
}

async fn find_child(
    storage: &Storage,
    parent: Option<ObjectHandle>,
    name: &str,
) -> Result<Option<ObjectInfo>, AppError> {
    Ok(crate::remote::list_object_infos(storage, parent)
        .await?
        .into_iter()
        .find(|object| object.filename == name))
}

/// MTP names stop at 254 UTF-16 units and the temp suffix needs about 50,
/// so the kept part of the original name is clipped to this many units.
const TEMP_NAME_STEM_UTF16_UNITS: usize = 190;

fn clip_to_utf16_units(text: &str, max_units: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= max_units
        })
        .collect()
}

fn replacement_temp_name(final_name: &str, label: &str, attempt: u32) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let clipped_name = clip_to_utf16_units(final_name, TEMP_NAME_STEM_UTF16_UNITS);

    format!(
        "{clipped_name}.tp7cli-{label}-{}-{stamp}-{attempt}.tmp",
        std::process::id()
    )
}

fn parent_remote_path(path: &str) -> String {
    path.rsplit_once('/').map_or_else(
        || "/".to_string(),
        |(parent, _)| {
            if parent.is_empty() {
                "/".to_string()
            } else {
                parent.to_string()
            }
        },
    )
}

async fn cleanup_uploaded_temp(storage: &Storage, handle: ObjectHandle) {
    let _ = storage.delete(handle).await;
}

fn sorted_directory_entries(path: &Path) -> Result<Vec<PathBuf>, AppError> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|error| AppError::FileSystem {
            path: path_to_string(path),
            message: error.to_string(),
        })?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| AppError::FileSystem {
                    path: path_to_string(path),
                    message: error.to_string(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    entries.sort_by_key(|path| path_to_string(path));
    Ok(entries)
}

fn file_report(
    local_path: &Path,
    remote_path: &str,
    size: u64,
    status: PushStatus,
) -> PushFileReport {
    PushFileReport {
        local_path: path_to_string(local_path),
        remote_path: remote_path.to_string(),
        size,
        status,
    }
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

    eprint!("\ruploaded {transferred}/{total} bytes ({percent:.1}%)");
    let _ = io::stderr().flush();
}

fn finish_progress(enabled: bool) {
    if enabled {
        eprintln!();
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

struct FileChunkStream {
    file: File,
    chunk_size: usize,
}

impl FileChunkStream {
    fn open(path: &Path) -> Result<Self, AppError> {
        let file = File::open(path).map_err(|error| AppError::FileSystem {
            path: path_to_string(path),
            message: error.to_string(),
        })?;

        Ok(Self {
            file,
            chunk_size: UPLOAD_CHUNK_SIZE,
        })
    }
}

impl Stream for FileChunkStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut buffer = vec![0; self.chunk_size];
        match self.file.read(&mut buffer) {
            Ok(0) => Poll::Ready(None),
            Ok(bytes_read) => {
                buffer.truncate(bytes_read);
                Poll::Ready(Some(Ok(Bytes::from(buffer))))
            }
            Err(error) => Poll::Ready(Some(Err(error))),
        }
    }
}

#[cfg(test)]
mod tests {
    use mtp_rs::ObjectHandle;

    use super::*;

    #[test]
    fn maps_root_parent_to_none() {
        assert_eq!(parent_handle(ObjectHandle(0)), None);
        assert_eq!(parent_handle(ObjectHandle(7)), Some(ObjectHandle(7)));
    }

    #[test]
    fn gets_local_file_name() {
        assert_eq!(
            local_file_name(Path::new("target/test.txt")).unwrap(),
            "test.txt"
        );
    }

    #[test]
    fn builds_parent_remote_paths() {
        assert_eq!(parent_remote_path("/file.txt"), "/");
        assert_eq!(parent_remote_path("/memo/file.txt"), "/memo");
    }

    #[test]
    fn builds_replacement_temp_names_with_original_prefix() {
        let name = replacement_temp_name("recording.wav", "upload", 3);

        assert!(name.starts_with("recording.wav.tp7cli-upload-"));
        assert!(name.ends_with("-3.tmp"));
    }

    #[test]
    fn staged_uploads_keep_the_final_names_format() {
        let staged = staged_object_info("take.wav.tp7cli-upload-1-2-0.tmp", "take.wav", 42);
        let direct = NewObjectInfo::file("take.wav", 42);

        assert_eq!(staged.filename, "take.wav.tp7cli-upload-1-2-0.tmp");
        assert_eq!(staged.format, direct.format);
        assert_ne!(
            staged.format,
            NewObjectInfo::file("take.wav.tp7cli-upload-1-2-0.tmp", 42).format
        );
    }

    #[test]
    fn keeps_temp_names_inside_the_mtp_name_limit() {
        let long_name = format!("{}.wav", "\u{1f3b5}".repeat(150));
        let name = replacement_temp_name(&long_name, "upload", 31);

        assert!(name.encode_utf16().count() <= 254);
        assert!(name.ends_with("-31.tmp"));
        assert_eq!(clip_to_utf16_units("a\u{1f3b5}", 2), "a");
    }
}
