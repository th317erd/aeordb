//! Shared private-path and capacity primitives for external v4 workspaces.

use std::fs;
use std::path::{Component, Path};

#[cfg(windows)]
use std::mem::size_of;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use thiserror::Error;

use crate::engine::emergency_spill::reject_symlink;
#[cfg(not(windows))]
use crate::engine::emergency_spill::create_private_dir;
use crate::engine::native_durability::{NativeDurabilityError, sync_directory_native};

#[derive(Debug, Error)]
pub(crate) enum PrivateWorkspaceErrorV1 {
  #[error("private workspace path is invalid or unavailable: {0}")]
  Path(String),
  #[cfg(windows)]
  #[error("private workspace state is invalid: {0}")]
  State(String),
  #[error("private workspace capacity is unavailable: {0}")]
  Capacity(String),
  #[cfg(windows)]
  #[error("private workspace allocation failed: {0}")]
  Allocation(String),
  #[error("private workspace I/O failed during {operation}: {source}")]
  Io {
    operation: &'static str,
    #[source]
    source: std::io::Error,
  },
  #[error("private workspace durability failed: {0}")]
  Durability(#[source] Box<NativeDurabilityError>),
}

pub(crate) fn is_canonical_lexical_absolute_utf8_path(path: &Path) -> bool {
  if !path.is_absolute()
    || path.to_str().is_none()
    || path.components().any(|component| matches!(component, Component::CurDir | Component::ParentDir))
  {
    return false;
  }
  #[cfg(windows)]
  {
    let path_text = path.to_str().expect("UTF-8 path checked above");
    if path_text.split(|character| character == '\\' || character == '/').any(|segment| segment == "." || segment == "..") {
      return false;
    }
  }
  true
}

pub(crate) fn validate_private_directory_readonly(path: &Path, role: &str) -> Result<(), PrivateWorkspaceErrorV1> {
  validate_private_directory(path, role)
}

pub(crate) fn validate_private_regular_file(path: &Path, file: &fs::File, role: &str) -> Result<(), PrivateWorkspaceErrorV1> {
  let metadata = file.metadata().map_err(|source| PrivateWorkspaceErrorV1::Io { operation: "private regular-file metadata", source })?;
  if !metadata.is_file() {
    return Err(PrivateWorkspaceErrorV1::Path(format!("{role} is not a regular file: {}", path.display())));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
      return Err(PrivateWorkspaceErrorV1::Path(format!("{role} is not private (mode {:04o}): {}", mode & 0o7777, path.display())));
    }
  }
  #[cfg(windows)]
  validate_windows_private_file_security(file, WindowsPrivateObjectKind::RegularFile, role, path)?;
  Ok(())
}

pub(crate) fn validate_regular_database_path(path: &Path, role: &str) -> Result<(), PrivateWorkspaceErrorV1> {
  if !path.is_absolute() {
    return Err(PrivateWorkspaceErrorV1::Path(format!("{role} path must be absolute")));
  }
  reject_symlink(path, role).map_err(|error| PrivateWorkspaceErrorV1::Path(error.to_string()))?;
  if !path.metadata().map_err(|source| PrivateWorkspaceErrorV1::Io { operation: "database metadata", source })?.is_file() {
    return Err(PrivateWorkspaceErrorV1::Path(format!("{role} path is not a regular file")));
  }
  let parent = path.parent().ok_or_else(|| PrivateWorkspaceErrorV1::Path(format!("{role} path has no parent")))?;
  validate_existing_directory(parent, "database parent")
}

pub(crate) fn validate_existing_directory(path: &Path, role: &str) -> Result<(), PrivateWorkspaceErrorV1> {
  reject_symlink(path, role).map_err(|error| PrivateWorkspaceErrorV1::Path(error.to_string()))?;
  if !fs::metadata(path).map_err(|source| PrivateWorkspaceErrorV1::Io { operation: "directory metadata", source })?.is_dir() {
    return Err(PrivateWorkspaceErrorV1::Path(format!("{role} is not a directory: {}", path.display())));
  }
  Ok(())
}

pub(crate) fn create_private_directory_synced(path: &Path, parent: &Path) -> Result<(), PrivateWorkspaceErrorV1> {
  create_platform_private_directory(path)?;
  validate_private_directory(path, "owned workspace directory")?;
  sync_directory_native(parent).map_err(|error| PrivateWorkspaceErrorV1::Durability(Box::new(error)))
}

pub(crate) fn secure_platform_private_directory(path: &Path) -> Result<(), PrivateWorkspaceErrorV1> {
  #[cfg(unix)]
  fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    .map_err(|source| PrivateWorkspaceErrorV1::Io { operation: "private directory permissions", source })?;
  #[cfg(windows)]
  set_windows_private_security(path, WindowsPrivateObjectKind::Directory, "private directory security")?;
  validate_private_directory(path, "secured private directory")
}

pub(crate) fn create_private_regular_file(path: &Path, role: &str) -> Result<fs::File, PrivateWorkspaceErrorV1> {
  let mut options = fs::OpenOptions::new();
  options.create_new(true).read(true).write(true);
  #[cfg(unix)]
  options.mode(0o600);
  let file = options.open(path).map_err(|source| PrivateWorkspaceErrorV1::Io { operation: "private regular-file creation", source })?;
  secure_platform_private_regular_file(path)?;
  validate_private_regular_file(path, &file, role)?;
  Ok(file)
}

pub(crate) fn validate_private_directory(path: &Path, role: &str) -> Result<(), PrivateWorkspaceErrorV1> {
  validate_existing_directory(path, role)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
      .map_err(|source| PrivateWorkspaceErrorV1::Io { operation: "private directory metadata", source })?
      .permissions()
      .mode();
    if mode & 0o077 != 0 {
      return Err(PrivateWorkspaceErrorV1::Path(format!("{role} is not private (mode {:04o}): {}", mode & 0o7777, path.display())));
    }
  }
  #[cfg(windows)]
  validate_windows_private_directory_security(path, role)?;
  Ok(())
}

#[cfg(not(windows))]
pub(crate) fn secure_platform_private_regular_file(_path: &Path) -> Result<(), PrivateWorkspaceErrorV1> {
  Ok(())
}

#[cfg(windows)]
pub(crate) fn secure_platform_private_regular_file(path: &Path) -> Result<(), PrivateWorkspaceErrorV1> {
  set_windows_private_security(path, WindowsPrivateObjectKind::RegularFile, "private regular-file security")
}

#[cfg(not(windows))]
fn create_platform_private_directory(path: &Path) -> Result<(), PrivateWorkspaceErrorV1> {
  create_private_dir(path).map_err(|error| PrivateWorkspaceErrorV1::Path(error.to_string()))
}

#[cfg(windows)]
fn create_platform_private_directory(path: &Path) -> Result<(), PrivateWorkspaceErrorV1> {
  use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
  use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

  let descriptor = WindowsPrivateSecurityDescriptor::new(WindowsPrivateObjectKind::Directory)?;
  let path = windows_path(path)?;
  let attributes = SECURITY_ATTRIBUTES {
    nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
      .map_err(|_| PrivateWorkspaceErrorV1::Capacity("Windows security attributes exceed u32".to_string()))?,
    lpSecurityDescriptor: descriptor.as_ptr(),
    bInheritHandle: 0,
  };
  if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
    let source = std::io::Error::last_os_error();
    if source.kind() == std::io::ErrorKind::AlreadyExists {
      return Err(PrivateWorkspaceErrorV1::Path(format!("private workspace directory already exists: {source}")));
    }
    return Err(PrivateWorkspaceErrorV1::Io { operation: "private directory creation", source });
  }
  Ok(())
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsPrivateObjectKind {
  Directory,
  RegularFile,
}

#[cfg(windows)]
fn set_windows_private_security(
  path: &Path,
  kind: WindowsPrivateObjectKind,
  operation: &'static str,
) -> Result<(), PrivateWorkspaceErrorV1> {
  use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
  };

  let descriptor = WindowsPrivateSecurityDescriptor::new(kind)?;
  let path = windows_path(path)?;
  let security_information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
  if unsafe { SetFileSecurityW(path.as_ptr(), security_information, descriptor.as_ptr()) } == 0 {
    return Err(PrivateWorkspaceErrorV1::Io { operation, source: std::io::Error::last_os_error() });
  }
  Ok(())
}

#[cfg(windows)]
fn validate_windows_private_directory_security(path: &Path, role: &str) -> Result<(), PrivateWorkspaceErrorV1> {
  use std::fs::OpenOptions;
  use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
  use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
  };

  let directory = OpenOptions::new()
    .read(true)
    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
    .open(path)
    .map_err(|source| PrivateWorkspaceErrorV1::Io { operation: "private directory security handle", source })?;
  let metadata =
    directory.metadata().map_err(|source| PrivateWorkspaceErrorV1::Io { operation: "private directory handle metadata", source })?;
  if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
    return Err(PrivateWorkspaceErrorV1::Path(format!("{role} is not a no-follow directory: {}", path.display())));
  }
  validate_windows_private_file_security(&directory, WindowsPrivateObjectKind::Directory, role, path)
}

#[cfg(windows)]
fn validate_windows_private_file_security(
  file: &fs::File,
  kind: WindowsPrivateObjectKind,
  role: &str,
  path: &Path,
) -> Result<(), PrivateWorkspaceErrorV1> {
  use std::os::windows::io::AsRawHandle;

  let expected = WindowsPrivateSecurityDescriptor::new(kind)?;
  let actual = WindowsFileSecurityDescriptor::read(file.as_raw_handle().cast())?;
  let expected_sddl = windows_security_descriptor_sddl(expected.as_ptr())?;
  let actual_sddl = windows_security_descriptor_sddl(actual.as_ptr())?;
  if actual_sddl != expected_sddl {
    return Err(PrivateWorkspaceErrorV1::Path(format!(
      "{role} is not private (expected {expected_sddl}, observed {actual_sddl}): {}",
      path.display()
    )));
  }
  Ok(())
}

#[cfg(windows)]
struct WindowsPrivateSecurityDescriptor(WindowsLocalAllocation);

#[cfg(windows)]
impl WindowsPrivateSecurityDescriptor {
  fn new(kind: WindowsPrivateObjectKind) -> Result<Self, PrivateWorkspaceErrorV1> {
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

    let current_user_sid = windows_current_user_sid()?;
    let inheritance = match kind {
      WindowsPrivateObjectKind::Directory => "OICI",
      WindowsPrivateObjectKind::RegularFile => "",
    };
    let sddl_capacity = current_user_sid
      .len()
      .checked_mul(2)
      .and_then(|length| length.checked_add(inheritance.len()))
      .and_then(|length| length.checked_add(43))
      .ok_or_else(|| PrivateWorkspaceErrorV1::Capacity("Windows private SDDL length overflow".to_string()))?;
    let mut sddl = String::new();
    sddl.try_reserve_exact(sddl_capacity).map_err(|error| PrivateWorkspaceErrorV1::Allocation(error.to_string()))?;
    sddl.push_str("O:");
    sddl.push_str(&current_user_sid);
    sddl.push_str("D:P(A;");
    sddl.push_str(inheritance);
    sddl.push_str(";FA;;;");
    sddl.push_str(&current_user_sid);
    sddl.push_str(")(A;");
    sddl.push_str(inheritance);
    sddl.push_str(";FA;;;SY)");
    let mut encoded_sddl = Vec::new();
    encoded_sddl.try_reserve_exact(sddl.len() + 1).map_err(|error| PrivateWorkspaceErrorV1::Allocation(error.to_string()))?;
    encoded_sddl.extend(sddl.encode_utf16());
    encoded_sddl.push(0);
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
      ConvertStringSecurityDescriptorToSecurityDescriptorW(
        encoded_sddl.as_ptr(),
        SECURITY_DESCRIPTOR_REVISION,
        &mut descriptor,
        std::ptr::null_mut(),
      )
    } == 0
    {
      return Err(PrivateWorkspaceErrorV1::Io { operation: "private security descriptor", source: std::io::Error::last_os_error() });
    }
    if descriptor.is_null() {
      return Err(PrivateWorkspaceErrorV1::State("Windows returned an empty private security descriptor".to_string()));
    }
    Ok(Self(WindowsLocalAllocation(descriptor)))
  }

  fn as_ptr(&self) -> *mut core::ffi::c_void {
    self.0 .0
  }
}

#[cfg(windows)]
struct WindowsFileSecurityDescriptor {
  words: Vec<usize>,
}

#[cfg(windows)]
impl WindowsFileSecurityDescriptor {
  fn read(handle: windows_sys::Win32::Foundation::HANDLE) -> Result<Self, PrivateWorkspaceErrorV1> {
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, GetKernelObjectSecurity, OWNER_SECURITY_INFORMATION};

    let requested = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut required_bytes = 0u32;
    let sizing_result = unsafe { GetKernelObjectSecurity(handle, requested, std::ptr::null_mut(), 0, &mut required_bytes) };
    if sizing_result == 0 {
      let source = std::io::Error::last_os_error();
      if source.raw_os_error() != Some(122) {
        return Err(PrivateWorkspaceErrorV1::Io { operation: "private security descriptor sizing", source });
      }
    }
    if required_bytes == 0 {
      return Err(PrivateWorkspaceErrorV1::State("Windows returned an empty private security descriptor size".to_string()));
    }
    let required_bytes_usize = usize::try_from(required_bytes)
      .map_err(|_| PrivateWorkspaceErrorV1::Capacity("Windows security descriptor exceeds usize".to_string()))?;
    let word_count = required_bytes_usize
      .checked_add(size_of::<usize>() - 1)
      .and_then(|bytes| bytes.checked_div(size_of::<usize>()))
      .ok_or_else(|| PrivateWorkspaceErrorV1::Capacity("Windows security descriptor allocation overflow".to_string()))?;
    let mut words = Vec::new();
    words.try_reserve_exact(word_count).map_err(|error| PrivateWorkspaceErrorV1::Allocation(error.to_string()))?;
    words.resize(word_count, 0usize);
    let allocated_bytes = word_count
      .checked_mul(size_of::<usize>())
      .ok_or_else(|| PrivateWorkspaceErrorV1::Capacity("Windows security descriptor allocation overflow".to_string()))?;
    let allocated_bytes = u32::try_from(allocated_bytes)
      .map_err(|_| PrivateWorkspaceErrorV1::Capacity("Windows security descriptor allocation exceeds u32".to_string()))?;
    let mut observed_bytes = required_bytes;
    if unsafe { GetKernelObjectSecurity(handle, requested, words.as_mut_ptr().cast(), allocated_bytes, &mut observed_bytes) } == 0 {
      return Err(PrivateWorkspaceErrorV1::Io {
        operation: "private security descriptor readback",
        source: std::io::Error::last_os_error(),
      });
    }
    if observed_bytes > allocated_bytes {
      return Err(PrivateWorkspaceErrorV1::State("Windows security descriptor grew after sizing".to_string()));
    }
    Ok(Self { words })
  }

  fn as_ptr(&self) -> *mut core::ffi::c_void {
    self.words.as_ptr().cast_mut().cast()
  }
}

#[cfg(windows)]
fn windows_security_descriptor_sddl(descriptor: *mut core::ffi::c_void) -> Result<String, PrivateWorkspaceErrorV1> {
  use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
  use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};
  use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

  let mut raw_sddl = std::ptr::null_mut();
  let mut length = 0u32;
  if unsafe {
    ConvertSecurityDescriptorToStringSecurityDescriptorW(
      descriptor,
      SECURITY_DESCRIPTOR_REVISION,
      OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
      &mut raw_sddl,
      &mut length,
    )
  } == 0
  {
    return Err(PrivateWorkspaceErrorV1::Io {
      operation: "private security descriptor normalization",
      source: std::io::Error::last_os_error(),
    });
  }
  if raw_sddl.is_null() || length == 0 {
    return Err(PrivateWorkspaceErrorV1::State("Windows returned an empty normalized security descriptor".to_string()));
  }
  let allocation = WindowsLocalAllocation(raw_sddl.cast());
  let maximum_length = usize::try_from(length)
    .map_err(|_| PrivateWorkspaceErrorV1::Capacity("Windows normalized security descriptor length exceeds usize".to_string()))?;
  let mut string_length = 0usize;
  while string_length < maximum_length && unsafe { *raw_sddl.add(string_length) } != 0 {
    string_length += 1;
  }
  if string_length == maximum_length {
    return Err(PrivateWorkspaceErrorV1::State("Windows normalized security descriptor is not NUL terminated".to_string()));
  }
  let sddl = String::from_utf16(unsafe { std::slice::from_raw_parts(raw_sddl, string_length) })
    .map_err(|_| PrivateWorkspaceErrorV1::Path("Windows normalized security descriptor is not valid UTF-16".to_string()))?;
  drop(allocation);
  Ok(sddl)
}

#[cfg(windows)]
fn windows_current_user_sid() -> Result<String, PrivateWorkspaceErrorV1> {
  use windows_sys::Win32::Foundation::HANDLE;
  use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
  use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
  use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

  let mut raw_token: HANDLE = std::ptr::null_mut();
  if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
    return Err(PrivateWorkspaceErrorV1::Io { operation: "workspace owner token", source: std::io::Error::last_os_error() });
  }
  if raw_token.is_null() {
    return Err(PrivateWorkspaceErrorV1::State("Windows returned an empty workspace owner token".to_string()));
  }
  let token = WindowsOwnedHandle(raw_token);
  let mut required_bytes = 0u32;
  let sizing_result = unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut required_bytes) };
  if sizing_result == 0 {
    let source = std::io::Error::last_os_error();
    if source.raw_os_error() != Some(122) {
      return Err(PrivateWorkspaceErrorV1::Io { operation: "workspace owner token sizing", source });
    }
  }
  let minimum_token_bytes = u32::try_from(size_of::<TOKEN_USER>())
    .map_err(|_| PrivateWorkspaceErrorV1::Capacity("Windows token header exceeds u32".to_string()))?;
  if required_bytes < minimum_token_bytes {
    return Err(PrivateWorkspaceErrorV1::State("Windows workspace owner token is shorter than TOKEN_USER".to_string()));
  }
  let required_bytes_usize =
    usize::try_from(required_bytes).map_err(|_| PrivateWorkspaceErrorV1::Capacity("Windows token size exceeds usize".to_string()))?;
  let word_count = required_bytes_usize
    .checked_add(size_of::<usize>() - 1)
    .and_then(|bytes| bytes.checked_div(size_of::<usize>()))
    .ok_or_else(|| PrivateWorkspaceErrorV1::Capacity("Windows token allocation size overflow".to_string()))?;
  let mut token_words = Vec::new();
  token_words.try_reserve_exact(word_count).map_err(|error| PrivateWorkspaceErrorV1::Allocation(error.to_string()))?;
  token_words.resize(word_count, 0usize);
  if unsafe { GetTokenInformation(token.0, TokenUser, token_words.as_mut_ptr().cast(), required_bytes, &mut required_bytes) } == 0 {
    return Err(PrivateWorkspaceErrorV1::Io { operation: "workspace owner token", source: std::io::Error::last_os_error() });
  }
  let token_user = unsafe { &*token_words.as_ptr().cast::<TOKEN_USER>() };
  if token_user.User.Sid.is_null() {
    return Err(PrivateWorkspaceErrorV1::State("Windows workspace owner token has no SID".to_string()));
  }
  let mut raw_sid_string = std::ptr::null_mut();
  if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut raw_sid_string) } == 0 {
    return Err(PrivateWorkspaceErrorV1::Io { operation: "workspace owner SID", source: std::io::Error::last_os_error() });
  }
  if raw_sid_string.is_null() {
    return Err(PrivateWorkspaceErrorV1::State("Windows returned an empty workspace owner SID".to_string()));
  }
  let sid_allocation = WindowsLocalAllocation(raw_sid_string.cast());
  let mut length = 0usize;
  while length < 256 && unsafe { *raw_sid_string.add(length) } != 0 {
    length += 1;
  }
  if length == 256 {
    return Err(PrivateWorkspaceErrorV1::Capacity("Windows workspace owner SID exceeds 255 UTF-16 units".to_string()));
  }
  let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(raw_sid_string, length) })
    .map_err(|_| PrivateWorkspaceErrorV1::Path("Windows workspace owner SID is not valid UTF-16".to_string()))?;
  drop(sid_allocation);
  Ok(sid)
}

#[cfg(windows)]
struct WindowsOwnedHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for WindowsOwnedHandle {
  fn drop(&mut self) {
    use windows_sys::Win32::Foundation::CloseHandle;
    if unsafe { CloseHandle(self.0) } == 0 {
      tracing::error!(error = %std::io::Error::last_os_error(), "Failed to close a private-workspace owner token");
    }
  }
}

#[cfg(windows)]
struct WindowsLocalAllocation(*mut core::ffi::c_void);

#[cfg(windows)]
impl Drop for WindowsLocalAllocation {
  fn drop(&mut self) {
    use windows_sys::Win32::Foundation::LocalFree;
    let retained = unsafe { LocalFree(self.0) };
    if !retained.is_null() {
      tracing::error!("Windows retained a private-workspace local allocation after LocalFree failed");
    }
  }
}

#[cfg(windows)]
fn windows_path(path: &Path) -> Result<Vec<u16>, PrivateWorkspaceErrorV1> {
  use std::os::windows::ffi::OsStrExt;
  let mut encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
  if encoded.contains(&0) {
    return Err(PrivateWorkspaceErrorV1::Path("Windows workspace path contains NUL".to_string()));
  }
  encoded.push(0);
  Ok(encoded)
}

pub(crate) fn ensure_capacity(path: &Path, additional_bytes: u64, minimum_free_bytes: u64) -> Result<(), PrivateWorkspaceErrorV1> {
  let required = additional_bytes
    .checked_add(minimum_free_bytes)
    .ok_or_else(|| PrivateWorkspaceErrorV1::Capacity("free-reserve arithmetic overflow".to_string()))?;
  let available = fs2::available_space(path).map_err(|source| PrivateWorkspaceErrorV1::Io { operation: "available space", source })?;
  if available < required {
    return Err(PrivateWorkspaceErrorV1::Capacity(format!("required {required} bytes but only {available} are available")));
  }
  Ok(())
}
