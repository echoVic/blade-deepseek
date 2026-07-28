use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::PlatformError;

pub struct ExclusiveFileLock {
    file: File,
    path: PathBuf,
}

impl ExclusiveFileLock {
    pub fn try_acquire(path: &Path) -> Result<Self, PlatformError> {
        Self::open_and_lock(path, LockMode::NonBlocking)
    }

    pub fn acquire(path: &Path) -> Result<Self, PlatformError> {
        Self::open_and_lock(path, LockMode::Blocking)
    }

    pub fn try_acquire_file(path: &Path, file: File) -> Result<Self, PlatformError> {
        Self::lock_open_file(path, file, LockMode::NonBlocking)
    }

    pub fn acquire_file(path: &Path, file: File) -> Result<Self, PlatformError> {
        Self::lock_open_file(path, file, LockMode::Blocking)
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open_and_lock(path: &Path, mode: LockMode) -> Result<Self, PlatformError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| PlatformError::io("create lock parent directory", error))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|error| PlatformError::io("open lock file", error))?;
        Self::lock_open_file(path, file, mode)
    }

    fn lock_open_file(path: &Path, file: File, mode: LockMode) -> Result<Self, PlatformError> {
        platform::lock(&file, path, mode)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }
}

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        platform::unlock(&self.file);
    }
}

#[derive(Clone, Copy)]
enum LockMode {
    Blocking,
    NonBlocking,
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::fd::AsRawFd;

    use super::*;

    pub(super) fn lock(file: &File, path: &Path, mode: LockMode) -> Result<(), PlatformError> {
        let operation = libc::LOCK_EX
            | if matches!(mode, LockMode::NonBlocking) {
                libc::LOCK_NB
            } else {
                0
            };
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if matches!(mode, LockMode::NonBlocking) && error.kind() == io::ErrorKind::WouldBlock {
                return Err(PlatformError::LockContended {
                    path: path.to_path_buf(),
                });
            }
            return Err(PlatformError::io("acquire exclusive file lock", error));
        }
    }

    pub(super) fn unlock(file: &File) {
        let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    use super::*;

    pub(super) fn lock(file: &File, path: &Path, mode: LockMode) -> Result<(), PlatformError> {
        let flags = LOCKFILE_EXCLUSIVE_LOCK
            | if matches!(mode, LockMode::NonBlocking) {
                LOCKFILE_FAIL_IMMEDIATELY
            } else {
                0
            };
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        let result = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                flags,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if result != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if matches!(mode, LockMode::NonBlocking)
            && error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32)
        {
            return Err(PlatformError::LockContended {
                path: path.to_path_buf(),
            });
        }
        Err(PlatformError::io("acquire exclusive file lock", error))
    }

    pub(super) fn unlock(file: &File) {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        let _ =
            unsafe { UnlockFileEx(file.as_raw_handle(), 0, u32::MAX, u32::MAX, &mut overlapped) };
    }
}
