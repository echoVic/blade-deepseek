use std::io;
use std::process::{Child, Command};

/// Owns the operating-system process-tree boundary for one spawned child.
/// Windows uses a Job Object with kill-on-close; other hosts keep this API as
/// a no-op because their process-group code remains authoritative.
#[derive(Debug)]
pub struct ProcessJob {
    platform: platform::ProcessJob,
}

impl ProcessJob {
    /// Spawns a child inside a new operating-system process-tree boundary.
    ///
    /// On Windows the child is created suspended, assigned to a Job Object,
    /// and only then resumed, so none of its code can run outside the job.
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        let (child, platform) = platform::ProcessJob::spawn(command, None)?;
        Ok((child, Self { platform }))
    }

    /// Spawns a child atomically inside a named Windows Job Object. The name
    /// allows a later Orca process to reopen and verify the ownership boundary.
    pub fn spawn_named(command: &mut Command, name: &str) -> io::Result<(Child, Self)> {
        let (child, platform) = platform::ProcessJob::spawn(command, Some(name))?;
        Ok((child, Self { platform }))
    }

    /// Spawns into an existing named Windows Job when the current process is
    /// already a member; otherwise creates and assigns the named boundary.
    /// This is used by the Windows runner so descendants inherit the runtime's
    /// Job Object instead of attempting an invalid second assignment.
    pub fn spawn_named_or_inherited(
        command: &mut Command,
        name: &str,
    ) -> io::Result<(Child, Self)> {
        let (child, platform) = platform::ProcessJob::spawn_named_or_inherited(command, name)?;
        Ok((child, Self { platform }))
    }

    /// Attaches an existing process to a new boundary.
    ///
    /// This cannot contain code that ran before assignment. Prefer [`Self::spawn`]
    /// unless the process is known to still be suspended or is being recovered.
    pub fn attach(pid: u32) -> io::Result<Self> {
        Ok(Self {
            platform: platform::ProcessJob::attach(pid)?,
        })
    }

    /// Attaches an existing process to a named Windows Job Object.
    /// Prefer [`Self::spawn_named`] for newly created children.
    pub fn attach_named(pid: u32, name: &str) -> io::Result<Self> {
        Ok(Self {
            platform: platform::ProcessJob::attach_named(pid, name)?,
        })
    }

    pub fn open_named(name: &str) -> io::Result<Self> {
        Ok(Self {
            platform: platform::ProcessJob::open_named(name)?,
        })
    }

    pub fn contains_process(&self, pid: u32) -> io::Result<bool> {
        self.platform.contains_process(pid)
    }

    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        self.platform.terminate(exit_code)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::io;
    use std::process::{Child, Command};

    #[derive(Debug)]
    pub(super) struct ProcessJob;

    impl ProcessJob {
        pub(super) fn spawn(
            command: &mut Command,
            _name: Option<&str>,
        ) -> io::Result<(Child, Self)> {
            Ok((command.spawn()?, Self))
        }

        pub(super) fn attach(_pid: u32) -> io::Result<Self> {
            Ok(Self)
        }

        pub(super) fn spawn_named_or_inherited(
            command: &mut Command,
            name: &str,
        ) -> io::Result<(Child, Self)> {
            Self::spawn(command, Some(name))
        }

        pub(super) fn attach_named(pid: u32, _name: &str) -> io::Result<Self> {
            Self::attach(pid)
        }

        pub(super) fn open_named(_name: &str) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "named process jobs are only available on Windows",
            ))
        }

        pub(super) fn contains_process(&self, _pid: u32) -> io::Result<bool> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "process job membership is only available on Windows",
            ))
        }

        pub(super) fn terminate(&self, _exit_code: u32) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, OpenJobObjectW, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows_sys::Win32::System::SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE};
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, GetCurrentProcess, OpenProcess, OpenThread,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE, ResumeThread,
        THREAD_SUSPEND_RESUME,
    };

    #[derive(Debug)]
    pub(super) struct ProcessJob {
        handle: HANDLE,
    }

    // Job Object handles are kernel-owned and may be closed or used from any
    // thread. This lets process owners move the lifetime guard to reaper
    // threads without transferring any borrowed memory.
    unsafe impl Send for ProcessJob {}

    impl ProcessJob {
        pub(super) fn spawn(
            command: &mut Command,
            name: Option<&str>,
        ) -> io::Result<(Child, Self)> {
            let job = Self::create(name)?;
            command.creation_flags(CREATE_SUSPENDED);
            let mut child = command.spawn()?;
            let process = child.as_raw_handle().cast();

            if unsafe { AssignProcessToJobObject(job.handle, process) } == 0 {
                let error = io::Error::last_os_error();
                terminate_suspended_child(&mut child, &job);
                return Err(error);
            }
            if let Err(error) = resume_process_threads(child.id()) {
                terminate_suspended_child(&mut child, &job);
                return Err(error);
            }

            Ok((child, job))
        }

        pub(super) fn spawn_named_or_inherited(
            command: &mut Command,
            name: &str,
        ) -> io::Result<(Child, Self)> {
            match Self::open_named(name) {
                Ok(job) if job.contains_current_process()? => Ok((command.spawn()?, job)),
                Ok(_) => Self::spawn(command, Some(name)),
                Err(error) if error.raw_os_error() == Some(2) => Self::spawn(command, Some(name)),
                Err(error) => Err(error),
            }
        }

        pub(super) fn attach(pid: u32) -> io::Result<Self> {
            Self::attach_with_name(pid, None)
        }

        pub(super) fn attach_named(pid: u32, name: &str) -> io::Result<Self> {
            Self::attach_with_name(pid, Some(name))
        }

        fn attach_with_name(pid: u32, name: Option<&str>) -> io::Result<Self> {
            let process = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE,
                    0,
                    pid,
                )
            };
            if process.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = match Self::create(name) {
                Ok(job) => job,
                Err(error) => {
                    unsafe { CloseHandle(process) };
                    return Err(error);
                }
            };
            let assigned = unsafe { AssignProcessToJobObject(job.handle, process) } != 0;
            unsafe { CloseHandle(process) };
            if !assigned {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        fn create(name: Option<&str>) -> io::Result<Self> {
            if let Some(name) = name {
                validate_name(name)?;
            }
            let wide_name = name.map(to_wide_name);
            let handle = unsafe {
                CreateJobObjectW(
                    std::ptr::null(),
                    wide_name
                        .as_ref()
                        .map_or(std::ptr::null(), |name| name.as_ptr()),
                )
            };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                unsafe { CloseHandle(handle) };
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle })
        }

        pub(super) fn open_named(name: &str) -> io::Result<Self> {
            validate_name(name)?;
            let wide_name = to_wide_name(name);
            let job = unsafe {
                OpenJobObjectW(
                    JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE,
                    0,
                    wide_name.as_ptr(),
                )
            };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle: job })
        }

        pub(super) fn contains_process(&self, pid: u32) -> io::Result<bool> {
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if process.is_null() {
                return Ok(false);
            }
            let mut result = 0;
            let inspected = unsafe { IsProcessInJob(process, self.handle, &mut result) };
            unsafe { CloseHandle(process) };
            if inspected == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(result != 0)
            }
        }

        fn contains_current_process(&self) -> io::Result<bool> {
            let mut result = 0;
            let inspected =
                unsafe { IsProcessInJob(GetCurrentProcess(), self.handle, &mut result) };
            if inspected == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(result != 0)
            }
        }

        pub(super) fn terminate(&self, exit_code: u32) -> io::Result<()> {
            if unsafe { TerminateJobObject(self.handle, exit_code) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    fn validate_name(name: &str) -> io::Result<()> {
        if name.is_empty() || name.encode_utf16().any(|unit| unit == 0) {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process job name must be non-empty and contain no NUL characters",
            ))
        } else {
            Ok(())
        }
    }

    fn to_wide_name(name: &str) -> Vec<u16> {
        name.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    }

    fn terminate_suspended_child(child: &mut Child, job: &ProcessJob) {
        let _ = job.terminate(1);
        let _ = child.kill();
        let _ = child.wait();
    }

    fn resume_process_threads(pid: u32) -> io::Result<()> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let result = resume_process_threads_from_snapshot(snapshot, pid);
        unsafe { CloseHandle(snapshot) };
        result
    }

    fn resume_process_threads_from_snapshot(snapshot: HANDLE, pid: u32) -> io::Result<()> {
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        if unsafe { Thread32First(snapshot, &mut entry) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut resumed = 0usize;
        loop {
            if entry.th32OwnerProcessID == pid {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let previous_count = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if previous_count == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed += 1;
            }
            if unsafe { Thread32Next(snapshot, &mut entry) } == 0 {
                break;
            }
        }

        if resumed == 0 {
            Err(io::Error::other(
                "spawned Windows process had no resumable threads",
            ))
        } else {
            Ok(())
        }
    }

    impl Drop for ProcessJob {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }
}
