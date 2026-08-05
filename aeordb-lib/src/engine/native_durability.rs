//! Native durability operations shared by current and v4 storage paths.
//!
//! Unsupported operations are returned explicitly. They are never converted
//! into warning-success, because later cutover and publication admission must
//! distinguish a proven barrier from an unavailable one.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use fs2::FileExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeDurabilityErrorClass {
  Unsupported,
  Io,
  UncertainCompletion,
  Verification,
  InvalidInput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeDurabilityOperation {
  DataBarrier,
  FileBarrier,
  ParentDirectorySync,
  DurableReplace,
  Preallocation,
  ReadBack,
  FileIdentity,
  FilesystemReport,
}

impl NativeDurabilityOperation {
  fn name(self) -> &'static str {
    match self {
      Self::DataBarrier => "data_barrier",
      Self::FileBarrier => "file_barrier",
      Self::ParentDirectorySync => "parent_directory_sync",
      Self::DurableReplace => "durable_replace",
      Self::Preallocation => "preallocation",
      Self::ReadBack => "read_back",
      Self::FileIdentity => "file_identity",
      Self::FilesystemReport => "filesystem_report",
    }
  }
}

#[derive(Debug)]
pub struct NativeDurabilityError {
  operation: NativeDurabilityOperation,
  class: NativeDurabilityErrorClass,
  message: String,
  source: Option<io::Error>,
}

impl NativeDurabilityError {
  pub fn class(&self) -> NativeDurabilityErrorClass {
    self.class
  }

  pub fn operation(&self) -> NativeDurabilityOperation {
    self.operation
  }

  pub fn is_unsupported(&self) -> bool {
    self.class == NativeDurabilityErrorClass::Unsupported
  }

  fn io(operation: NativeDurabilityOperation, source: io::Error) -> Self {
    Self { operation, class: NativeDurabilityErrorClass::Io, message: source.to_string(), source: Some(source) }
  }

  fn operation_io(operation: NativeDurabilityOperation, source: io::Error) -> Self {
    if is_unsupported_io(&source) {
      Self { operation, class: NativeDurabilityErrorClass::Unsupported, message: source.to_string(), source: Some(source) }
    } else {
      Self::io(operation, source)
    }
  }

  fn unsupported(operation: NativeDurabilityOperation, message: impl Into<String>) -> Self {
    Self { operation, class: NativeDurabilityErrorClass::Unsupported, message: message.into(), source: None }
  }

  fn verification(operation: NativeDurabilityOperation, message: impl Into<String>) -> Self {
    Self { operation, class: NativeDurabilityErrorClass::Verification, message: message.into(), source: None }
  }

  fn uncertain(operation: NativeDurabilityOperation, cause: Self) -> Self {
    Self {
      operation,
      class: NativeDurabilityErrorClass::UncertainCompletion,
      message: format!("namespace mutation may have completed before the durability barrier failed: {cause}"),
      source: cause.source,
    }
  }

  fn invalid(operation: NativeDurabilityOperation, message: impl Into<String>) -> Self {
    Self { operation, class: NativeDurabilityErrorClass::InvalidInput, message: message.into(), source: None }
  }
}

impl fmt::Display for NativeDurabilityError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{} {:?}: {}", self.operation.name(), self.class, self.message)
  }
}

impl std::error::Error for NativeDurabilityError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    self.source.as_ref().map(|source| source as &(dyn std::error::Error + 'static))
  }
}

pub type NativeDurabilityResult<T> = Result<T, NativeDurabilityError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeOperationSupport {
  Supported,
  Unsupported { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeDurabilityCapabilities {
  pub data_barrier: NativeOperationSupport,
  pub file_barrier: NativeOperationSupport,
  pub parent_directory_sync: NativeOperationSupport,
  pub durable_replace: NativeOperationSupport,
  pub preallocation: NativeOperationSupport,
  pub stable_file_identity: NativeOperationSupport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeFilesystemInfo {
  pub kind: String,
  pub flags: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformFileIdentityDescriptorV1 {
  pub platform: u16,
  pub schema: u16,
  pub flags: u32,
  pub volume_identity: [u8; 16],
  pub file_identity: [u8; 16],
  pub birth_identity: [u8; 16],
}

impl PlatformFileIdentityDescriptorV1 {
  pub const ENCODED_LENGTH: usize = 56;

  pub fn to_bytes(self) -> [u8; Self::ENCODED_LENGTH] {
    let mut bytes = [0u8; Self::ENCODED_LENGTH];
    bytes[..2].copy_from_slice(&self.platform.to_le_bytes());
    bytes[2..4].copy_from_slice(&self.schema.to_le_bytes());
    bytes[4..8].copy_from_slice(&self.flags.to_le_bytes());
    bytes[8..24].copy_from_slice(&self.volume_identity);
    bytes[24..40].copy_from_slice(&self.file_identity);
    bytes[40..56].copy_from_slice(&self.birth_identity);
    bytes
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeDurabilityProbeReport {
  pub filesystem: NativeFilesystemInfo,
  pub capabilities: NativeDurabilityCapabilities,
  pub read_back_verified: bool,
  pub identity_before_rename: Option<PlatformFileIdentityDescriptorV1>,
  pub identity_after_rename: Option<PlatformFileIdentityDescriptorV1>,
  pub destination_identity_before_replace: Option<PlatformFileIdentityDescriptorV1>,
  pub replaced_identity: Option<PlatformFileIdentityDescriptorV1>,
}

pub fn sync_file_data_native(file: &File) -> NativeDurabilityResult<()> {
  sync_data_platform(file).map_err(|error| NativeDurabilityError::operation_io(NativeDurabilityOperation::DataBarrier, error))
}

pub fn sync_file_all_native(file: &File) -> NativeDurabilityResult<()> {
  sync_all_platform(file).map_err(|error| NativeDurabilityError::operation_io(NativeDurabilityOperation::FileBarrier, error))
}

pub fn sync_directory_native(path: impl AsRef<Path>) -> NativeDurabilityResult<()> {
  sync_directory_platform(path.as_ref())
}

pub fn preallocate_file(file: &File, length: u64) -> NativeDurabilityResult<()> {
  if length == 0 {
    return Err(NativeDurabilityError::invalid(NativeDurabilityOperation::Preallocation, "preallocation length must be nonzero"));
  }
  FileExt::allocate(file, length).map_err(|error| NativeDurabilityError::operation_io(NativeDurabilityOperation::Preallocation, error))
}

pub fn durable_replace_native(from: impl AsRef<Path>, to: impl AsRef<Path>) -> NativeDurabilityResult<()> {
  let from = from.as_ref();
  let to = to.as_ref();
  if from == to {
    return Err(NativeDurabilityError::invalid(NativeDurabilityOperation::DurableReplace, "source and destination must differ"));
  }
  // Probe the required namespace barrier before mutation. If it is unsupported,
  // the caller gets a clean refusal rather than an already-renamed path.
  sync_rename_parents(from, to)?;
  durable_replace_platform(from, to)?;
  sync_rename_parents(from, to).map_err(|error| NativeDurabilityError::uncertain(NativeDurabilityOperation::DurableReplace, error))
}

pub fn platform_file_identity(path: impl AsRef<Path>) -> NativeDurabilityResult<PlatformFileIdentityDescriptorV1> {
  platform_file_identity_impl(path.as_ref())
}

pub fn probe_native_durability(root: impl AsRef<Path>) -> NativeDurabilityResult<NativeDurabilityProbeReport> {
  let root = root.as_ref();
  let root_metadata =
    std::fs::symlink_metadata(root).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::FilesystemReport, error))?;
  if !root_metadata.file_type().is_dir() {
    return Err(NativeDurabilityError::invalid(NativeDurabilityOperation::FilesystemReport, "probe root must be an existing directory"));
  }
  let filesystem = native_filesystem_info(root)?;
  let probe_id = uuid::Uuid::new_v4();
  let source = root.join(format!(".aeordb-native-probe-{probe_id}-source"));
  let moved = root.join(format!(".aeordb-native-probe-{probe_id}-moved"));
  let destination = root.join(format!(".aeordb-native-probe-{probe_id}-destination"));

  let mut file = OpenOptions::new()
    .create_new(true)
    .read(true)
    .write(true)
    .open(&source)
    .map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ReadBack, error))?;
  let preallocation = probe_support(preallocate_file(&file, 64 * 1024))?;
  file.seek(SeekFrom::Start(0)).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ReadBack, error))?;
  file
    .write_all(b"aeordb-native-durability-probe-v1")
    .map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ReadBack, error))?;
  let data_barrier = probe_support(sync_file_data_native(&file))?;
  let file_barrier = probe_support(sync_file_all_native(&file))?;
  drop(file);

  let mut read_back = Vec::new();
  File::open(&source)
    .and_then(|mut file| file.read_to_end(&mut read_back))
    .map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ReadBack, error))?;
  if !read_back.starts_with(b"aeordb-native-durability-probe-v1") {
    return Err(NativeDurabilityError::verification(NativeDurabilityOperation::ReadBack, "barrier read-back bytes differ"));
  }

  let identity_before_rename = probe_value(platform_file_identity(&source))?;
  std::fs::rename(&source, &moved).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::DurableReplace, error))?;
  let parent_directory_sync = probe_support(sync_directory_native(root))?;
  let identity_after_rename = probe_value(platform_file_identity(&moved))?;
  if let (Some(before), Some(after)) = (identity_before_rename, identity_after_rename) {
    if before != after {
      return Err(NativeDurabilityError::verification(NativeDurabilityOperation::FileIdentity, "identity changed across rename"));
    }
  }

  let mut old_destination =
    File::create(&destination).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ReadBack, error))?;
  old_destination.write_all(b"old destination").map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ReadBack, error))?;
  let destination_barrier = probe_support(sync_file_all_native(&old_destination))?;
  if destination_barrier != NativeOperationSupport::Supported && file_barrier == NativeOperationSupport::Supported {
    return Err(NativeDurabilityError::verification(
      NativeDurabilityOperation::FileBarrier,
      "file barrier capability changed during probe",
    ));
  }
  drop(old_destination);
  let destination_identity_before_replace = probe_value(platform_file_identity(&destination))?;
  let durable_replace = probe_support(durable_replace_native(&moved, &destination))?;
  let (replaced_identity, replace_readback) = if durable_replace == NativeOperationSupport::Supported {
    let bytes = std::fs::read(&destination).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ReadBack, error))?;
    if !bytes.starts_with(b"aeordb-native-durability-probe-v1") {
      return Err(NativeDurabilityError::verification(NativeDurabilityOperation::ReadBack, "durable replace read-back bytes differ"));
    }
    (probe_value(platform_file_identity(&destination))?, true)
  } else {
    (None, false)
  };

  let stable_file_identity = if identity_before_rename.is_some()
    && identity_after_rename.is_some()
    && destination_identity_before_replace.is_some()
    && (durable_replace != NativeOperationSupport::Supported || replaced_identity.is_some())
  {
    NativeOperationSupport::Supported
  } else {
    NativeOperationSupport::Unsupported { reason: "platform did not provide stable file identity evidence".to_string() }
  };

  remove_probe_file(&destination)?;
  remove_probe_file(&moved)?;
  Ok(NativeDurabilityProbeReport {
    filesystem,
    capabilities: NativeDurabilityCapabilities {
      data_barrier,
      file_barrier,
      parent_directory_sync,
      durable_replace,
      preallocation,
      stable_file_identity,
    },
    read_back_verified: replace_readback,
    identity_before_rename,
    identity_after_rename,
    destination_identity_before_replace,
    replaced_identity,
  })
}

fn remove_probe_file(path: &Path) -> NativeDurabilityResult<()> {
  match std::fs::remove_file(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(NativeDurabilityError::io(NativeDurabilityOperation::ReadBack, error)),
  }
}

fn probe_support(result: NativeDurabilityResult<()>) -> NativeDurabilityResult<NativeOperationSupport> {
  match result {
    Ok(()) => Ok(NativeOperationSupport::Supported),
    Err(error) if error.is_unsupported() => Ok(NativeOperationSupport::Unsupported { reason: error.to_string() }),
    Err(error) => Err(error),
  }
}

fn probe_value<T>(result: NativeDurabilityResult<T>) -> NativeDurabilityResult<Option<T>> {
  match result {
    Ok(value) => Ok(Some(value)),
    Err(error) if error.is_unsupported() => Ok(None),
    Err(error) => Err(error),
  }
}

fn sync_rename_parents(from: &Path, to: &Path) -> NativeDurabilityResult<()> {
  let from_parent = parent_or_current(from);
  let to_parent = parent_or_current(to);
  sync_directory_native(to_parent)?;
  if from_parent != to_parent {
    sync_directory_native(from_parent)?;
  }
  Ok(())
}

fn parent_or_current(path: &Path) -> &Path {
  path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."))
}

#[cfg(not(target_os = "macos"))]
fn sync_data_platform(file: &File) -> io::Result<()> {
  file.sync_data()
}

#[cfg(target_os = "macos")]
fn sync_data_platform(file: &File) -> io::Result<()> {
  use std::os::fd::AsRawFd;
  if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) } == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(not(target_os = "macos"))]
fn sync_all_platform(file: &File) -> io::Result<()> {
  file.sync_all()
}

#[cfg(target_os = "macos")]
fn sync_all_platform(file: &File) -> io::Result<()> {
  use std::os::fd::AsRawFd;
  if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(unix)]
fn sync_directory_platform(path: &Path) -> NativeDurabilityResult<()> {
  let metadata =
    std::fs::symlink_metadata(path).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ParentDirectorySync, error))?;
  if !metadata.file_type().is_dir() {
    return Err(NativeDurabilityError::invalid(
      NativeDurabilityOperation::ParentDirectorySync,
      "directory barrier target is not a directory",
    ));
  }
  let directory = File::open(path).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ParentDirectorySync, error))?;
  sync_all_platform(&directory).map_err(|error| NativeDurabilityError::operation_io(NativeDurabilityOperation::ParentDirectorySync, error))
}

#[cfg(windows)]
fn sync_directory_platform(path: &Path) -> NativeDurabilityResult<()> {
  use std::os::windows::fs::OpenOptionsExt;
  use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
  };

  let directory = OpenOptions::new()
    .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE)
    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
    .open(path)
    .map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::ParentDirectorySync, error))?;
  directory.sync_all().map_err(|error| NativeDurabilityError::operation_io(NativeDurabilityOperation::ParentDirectorySync, error))
}

#[cfg(unix)]
fn durable_replace_platform(from: &Path, to: &Path) -> NativeDurabilityResult<()> {
  std::fs::rename(from, to).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::DurableReplace, error))
}

#[cfg(windows)]
fn durable_replace_platform(from: &Path, to: &Path) -> NativeDurabilityResult<()> {
  use std::os::windows::ffi::OsStrExt;
  use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACEFILE_WRITE_THROUGH, ReplaceFileW,
  };

  let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
  let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
  let result = unsafe {
    if to.exists() {
      ReplaceFileW(to_wide.as_ptr(), from_wide.as_ptr(), std::ptr::null(), REPLACEFILE_WRITE_THROUGH, std::ptr::null(), std::ptr::null())
    } else {
      MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
    }
  };
  if result == 0 {
    return Err(NativeDurabilityError::operation_io(NativeDurabilityOperation::DurableReplace, io::Error::last_os_error()));
  }
  Ok(())
}

#[cfg(unix)]
fn platform_file_identity_impl(path: &Path) -> NativeDurabilityResult<PlatformFileIdentityDescriptorV1> {
  use std::os::unix::fs::OpenOptionsExt;
  let file = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
    .open(path)
    .map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::FileIdentity, error))?;
  unix_file_identity(&file)
}

#[cfg(target_os = "linux")]
fn unix_file_identity(file: &File) -> NativeDurabilityResult<PlatformFileIdentityDescriptorV1> {
  use std::os::fd::AsRawFd;
  use std::os::unix::fs::MetadataExt;

  let metadata = file.metadata().map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::FileIdentity, error))?;
  let stat = linux_statfs(file.as_raw_fd(), NativeDurabilityOperation::FileIdentity)?;
  let volume_identity = volume_identity(metadata.dev(), fsid_bytes(&stat.f_fsid)?);
  let mut file_identity = [0u8; 16];
  file_identity[..8].copy_from_slice(&metadata.ino().to_le_bytes());
  let (birth_identity, has_birth) = metadata.created().map(encode_system_time).map_or(([0; 16], false), |value| (value, true));
  Ok(PlatformFileIdentityDescriptorV1 {
    platform: 1,
    schema: 1,
    flags: if has_birth { 1 << 1 } else { 0 },
    volume_identity,
    file_identity,
    birth_identity,
  })
}

#[cfg(target_os = "macos")]
fn unix_file_identity(file: &File) -> NativeDurabilityResult<PlatformFileIdentityDescriptorV1> {
  use std::os::fd::AsRawFd;
  use std::os::macos::fs::MetadataExt;

  let metadata = file.metadata().map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::FileIdentity, error))?;
  let stat = macos_statfs(file.as_raw_fd(), NativeDurabilityOperation::FileIdentity)?;
  let volume_identity = volume_identity(metadata.st_dev() as u64, fsid_bytes(&stat.f_fsid)?);
  let mut file_identity = [0u8; 16];
  file_identity[..8].copy_from_slice(&metadata.st_ino().to_le_bytes());
  let mut birth_identity = [0u8; 16];
  birth_identity[..8].copy_from_slice(&metadata.st_birthtime().to_le_bytes());
  birth_identity[8..].copy_from_slice(&metadata.st_birthtime_nsec().to_le_bytes());
  Ok(PlatformFileIdentityDescriptorV1 { platform: 1, schema: 1, flags: 1 << 1, volume_identity, file_identity, birth_identity })
}

#[cfg(windows)]
fn platform_file_identity_impl(path: &Path) -> NativeDurabilityResult<PlatformFileIdentityDescriptorV1> {
  use std::mem::size_of;
  use std::os::windows::fs::OpenOptionsExt;
  use std::os::windows::io::AsRawHandle;
  use windows_sys::Win32::Storage::FileSystem::{
    FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
  };

  let file = OpenOptions::new()
    .read(true)
    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
    .open(path)
    .map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::FileIdentity, error))?;
  let handle = file.as_raw_handle();
  let mut id = FILE_ID_INFO::default();
  if unsafe { GetFileInformationByHandleEx(handle, FileIdInfo, &mut id as *mut _ as *mut _, size_of::<FILE_ID_INFO>() as u32) } == 0 {
    return Err(NativeDurabilityError::operation_io(NativeDurabilityOperation::FileIdentity, io::Error::last_os_error()));
  }
  let mut basic = FILE_BASIC_INFO::default();
  if unsafe { GetFileInformationByHandleEx(handle, FileBasicInfo, &mut basic as *mut _ as *mut _, size_of::<FILE_BASIC_INFO>() as u32) }
    == 0
  {
    return Err(NativeDurabilityError::operation_io(NativeDurabilityOperation::FileIdentity, io::Error::last_os_error()));
  }
  let mut volume_identity = [0u8; 16];
  volume_identity[..8].copy_from_slice(&id.VolumeSerialNumber.to_le_bytes());
  let mut birth_identity = [0u8; 16];
  birth_identity[..8].copy_from_slice(&(basic.CreationTime as u64).to_le_bytes());
  Ok(PlatformFileIdentityDescriptorV1 {
    platform: 2,
    schema: 1,
    flags: 1 << 1,
    volume_identity,
    file_identity: id.FileId.Identifier,
    birth_identity,
  })
}

#[cfg(target_os = "linux")]
fn native_filesystem_info(path: &Path) -> NativeDurabilityResult<NativeFilesystemInfo> {
  use std::os::fd::AsRawFd;
  let file = File::open(path).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::FilesystemReport, error))?;
  let stat = linux_statfs(file.as_raw_fd(), NativeDurabilityOperation::FilesystemReport)?;
  Ok(NativeFilesystemInfo { kind: format!("linux:0x{:x}", stat.f_type as u64), flags: 0 })
}

#[cfg(target_os = "macos")]
fn native_filesystem_info(path: &Path) -> NativeDurabilityResult<NativeFilesystemInfo> {
  use std::os::fd::AsRawFd;
  let file = File::open(path).map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::FilesystemReport, error))?;
  let stat = macos_statfs(file.as_raw_fd(), NativeDurabilityOperation::FilesystemReport)?;
  let end = stat.f_fstypename.iter().position(|byte| *byte == 0).unwrap_or(stat.f_fstypename.len());
  let kind = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(stat.f_fstypename.as_ptr().cast::<u8>(), end) }).into_owned();
  Ok(NativeFilesystemInfo { kind, flags: stat.f_flags as u64 })
}

#[cfg(windows)]
fn native_filesystem_info(path: &Path) -> NativeDurabilityResult<NativeFilesystemInfo> {
  use std::os::windows::fs::OpenOptionsExt;
  use std::os::windows::io::AsRawHandle;
  use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationByHandleW,
  };

  let file = OpenOptions::new()
    .read(true)
    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
    .open(path)
    .map_err(|error| NativeDurabilityError::io(NativeDurabilityOperation::FilesystemReport, error))?;
  let mut filesystem = [0u16; 64];
  let mut flags = 0u32;
  if unsafe {
    GetVolumeInformationByHandleW(
      file.as_raw_handle(),
      std::ptr::null_mut(),
      0,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      &mut flags,
      filesystem.as_mut_ptr(),
      filesystem.len() as u32,
    )
  } == 0
  {
    return Err(NativeDurabilityError::operation_io(NativeDurabilityOperation::FilesystemReport, io::Error::last_os_error()));
  }
  let end = filesystem.iter().position(|value| *value == 0).unwrap_or(filesystem.len());
  Ok(NativeFilesystemInfo { kind: String::from_utf16_lossy(&filesystem[..end]), flags: u64::from(flags) })
}

#[cfg(target_os = "linux")]
fn linux_statfs(fd: std::os::fd::RawFd, operation: NativeDurabilityOperation) -> NativeDurabilityResult<libc::statfs> {
  let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
  if unsafe { libc::fstatfs(fd, stat.as_mut_ptr()) } == -1 {
    return Err(NativeDurabilityError::operation_io(operation, io::Error::last_os_error()));
  }
  Ok(unsafe { stat.assume_init() })
}

#[cfg(target_os = "macos")]
fn macos_statfs(fd: std::os::fd::RawFd, operation: NativeDurabilityOperation) -> NativeDurabilityResult<libc::statfs> {
  let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
  if unsafe { libc::fstatfs(fd, stat.as_mut_ptr()) } == -1 {
    return Err(NativeDurabilityError::operation_io(operation, io::Error::last_os_error()));
  }
  Ok(unsafe { stat.assume_init() })
}

#[cfg(unix)]
fn fsid_bytes(fsid: &libc::fsid_t) -> NativeDurabilityResult<[u8; 8]> {
  if std::mem::size_of::<libc::fsid_t>() != 8 {
    return Err(NativeDurabilityError::unsupported(
      NativeDurabilityOperation::FileIdentity,
      "platform fsid_t cannot be represented by PlatformFileIdentityDescriptorV1",
    ));
  }
  let mut native = [0u8; 8];
  unsafe { std::ptr::copy_nonoverlapping((fsid as *const libc::fsid_t).cast::<u8>(), native.as_mut_ptr(), native.len()) };
  let first = i32::from_ne_bytes([native[0], native[1], native[2], native[3]]);
  let second = i32::from_ne_bytes([native[4], native[5], native[6], native[7]]);
  let mut canonical = [0u8; 8];
  canonical[..4].copy_from_slice(&first.to_le_bytes());
  canonical[4..].copy_from_slice(&second.to_le_bytes());
  Ok(canonical)
}

#[cfg(unix)]
fn volume_identity(device: u64, fsid: [u8; 8]) -> [u8; 16] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(&device.to_le_bytes());
  hasher.update(&fsid);
  let mut identity = [0u8; 16];
  identity.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
  identity
}

#[cfg(target_os = "linux")]
fn encode_system_time(time: std::time::SystemTime) -> [u8; 16] {
  use std::time::UNIX_EPOCH;
  let (seconds, nanos) = match time.duration_since(UNIX_EPOCH) {
    Ok(duration) => (i64::try_from(duration.as_secs()).unwrap_or(i64::MAX), i64::from(duration.subsec_nanos())),
    Err(error) => {
      let duration = error.duration();
      (-i64::try_from(duration.as_secs()).unwrap_or(i64::MAX), -i64::from(duration.subsec_nanos()))
    }
  };
  let mut identity = [0u8; 16];
  identity[..8].copy_from_slice(&seconds.to_le_bytes());
  identity[8..].copy_from_slice(&nanos.to_le_bytes());
  identity
}

#[cfg(unix)]
fn is_unsupported_io(error: &io::Error) -> bool {
  matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EOPNOTSUPP | libc::EINVAL))
}

#[cfg(windows)]
fn is_unsupported_io(error: &io::Error) -> bool {
  matches!(error.raw_os_error(), Some(1 | 50 | 120))
}

#[cfg(not(any(unix, windows)))]
compile_error!("AeorDB native durability probes require an explicit platform implementation");
