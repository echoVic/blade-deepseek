use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::process::ExitStatus;

use orca_platform::process::ProcessJob;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, INVALID_HANDLE_VALUE,
    STILL_ACTIVE, SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::{
    SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::{
    AppContainerSecurity, CapabilityStore, PreparedSecurity, SandboxFilesystemMode,
    WindowsSandboxError, WindowsSandboxPlan, prepare_appcontainer_security, prepare_security,
};

const INFINITE: u32 = 0xffff_ffff;

pub struct SandboxSpawnRequest<'a> {
    pub program: &'a Path,
    pub args: &'a [OsString],
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, Option<String>>,
    pub plan: &'a WindowsSandboxPlan,
    pub capabilities: &'a CapabilityStore,
}

pub struct SandboxedChild {
    process: SandboxedProcess,
    stdin: Option<std::fs::File>,
    stdout: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
}

pub type SandboxStdio = (std::fs::File, Box<dyn Read + Send>, Box<dyn Read + Send>);

pub struct SandboxedPty {
    process: SandboxedProcess,
    input: Option<SandboxedPtyInput>,
    output: Option<std::fs::File>,
}

pub struct SandboxedPtyInput {
    console: Option<PseudoConsole>,
    writer: Option<std::fs::File>,
}

impl SandboxedChild {
    pub fn spawn(request: SandboxSpawnRequest<'_>) -> Result<Self, WindowsSandboxError> {
        let (restricted, appcontainer) = prepare_spawn_security(&request)?;
        let mut pipes = PipeSet::new()?;
        let mut attributes = ProcessAttributeList::new(1 + u32::from(appcontainer.is_some()))?;
        attributes.set_handle_list(vec![
            pipes.child_stdin,
            pipes.child_stdout,
            pipes.child_stderr,
        ])?;
        if let Some(appcontainer) = appcontainer.as_ref() {
            attributes.set_security_capabilities(
                appcontainer.app_sid(),
                appcontainer.capability_sids(),
            )?;
        }
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: pipes.child_stdin,
                hStdOutput: pipes.child_stdout,
                hStdError: pipes.child_stderr,
                ..unsafe { std::mem::zeroed() }
            },
            lpAttributeList: attributes.as_mut_ptr(),
        };
        let process = spawn_suspended(&request, restricted.as_ref(), &startup, true, false)?;
        pipes.close_child_ends();
        Ok(Self {
            process,
            stdin: Some(pipes.parent_stdin.take().expect("parent stdin")),
            stdout: Some(pipes.parent_stdout.take().expect("parent stdout")),
            stderr: Some(pipes.parent_stderr.take().expect("parent stderr")),
        })
    }

    pub fn id(&self) -> u32 {
        self.process.id()
    }

    pub fn take_process_job(&mut self) -> io::Result<ProcessJob> {
        self.process.take_process_job()
    }

    pub fn take_stdio(&mut self) -> io::Result<SandboxStdio> {
        let stdin = self
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "sandbox stdin closed"))?;
        let stdout = self
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("sandbox stdout already taken"))?;
        let stderr = self
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("sandbox stderr already taken"))?;
        Ok((stdin, Box::new(stdout), Box::new(stderr)))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.process.try_wait()
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.process.wait()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.process.kill()
    }
}

impl SandboxedPty {
    pub fn spawn(
        request: SandboxSpawnRequest<'_>,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<Self, WindowsSandboxError> {
        let (restricted, appcontainer) = prepare_spawn_security(&request)?;
        let pty = PtyPipeSet::new(cols, rows)?;
        let mut attributes = ProcessAttributeList::new(1 + u32::from(appcontainer.is_some()))?;
        attributes.set_pseudo_console(pty.console.raw())?;
        if let Some(appcontainer) = appcontainer.as_ref() {
            attributes.set_security_capabilities(
                appcontainer.app_sid(),
                appcontainer.capability_sids(),
            )?;
        }
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: INVALID_HANDLE_VALUE,
                hStdOutput: INVALID_HANDLE_VALUE,
                hStdError: INVALID_HANDLE_VALUE,
                ..unsafe { std::mem::zeroed() }
            },
            lpAttributeList: attributes.as_mut_ptr(),
        };
        let process = spawn_suspended(&request, restricted.as_ref(), &startup, false, true)?;
        let (input, output) = pty.into_io();
        Ok(Self {
            process,
            input: Some(input),
            output: Some(output),
        })
    }

    pub fn id(&self) -> u32 {
        self.process.id()
    }

    pub fn take_process_job(&mut self) -> io::Result<ProcessJob> {
        self.process.take_process_job()
    }

    pub fn take_pty(&mut self) -> io::Result<(SandboxedPtyInput, Box<dyn Read + Send>)> {
        let input = self
            .input
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "sandbox PTY is closed"))?;
        let output = self
            .output
            .take()
            .ok_or_else(|| io::Error::other("sandbox PTY output is closed"))?;
        Ok((input, Box::new(output)))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.process.try_wait()
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.process.wait()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.process.kill()
    }
}

impl Drop for SandboxedPty {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

impl SandboxedPtyInput {
    pub fn write_all(&mut self, input: &[u8]) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "sandbox PTY is closed"))?;
        writer.write_all(input)?;
        writer.flush()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.console
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "sandbox PTY is closed"))?
            .resize(cols, rows)
    }

    pub fn close_terminal(&mut self) {
        self.writer.take();
        self.console.take();
    }

    pub fn close(&mut self) {
        self.writer.take();
    }
}

struct SandboxedProcess {
    process: OwnedHandle,
    job: Option<ProcessJob>,
    pid: u32,
}

impl SandboxedProcess {
    fn id(&self) -> u32 {
        self.pid
    }

    fn take_process_job(&mut self) -> io::Result<ProcessJob> {
        self.job
            .take()
            .ok_or_else(|| io::Error::other("sandbox process job already taken"))
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut code = 0u32;
        if unsafe { GetExitCodeProcess(self.process.raw(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if code == STILL_ACTIVE as u32 {
            return Ok(None);
        }
        Ok(Some(exit_status(code)))
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        let result = unsafe { WaitForSingleObject(self.process.raw(), INFINITE) };
        if result != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        self.try_wait()?
            .ok_or_else(|| io::Error::other("process remained active after wait"))
    }

    fn kill(&mut self) -> io::Result<()> {
        if let Some(job) = &self.job {
            job.terminate(137)
        } else if unsafe { TerminateProcess(self.process.raw(), 137) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for SandboxedChild {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

fn prepare_spawn_security(
    request: &SandboxSpawnRequest<'_>,
) -> Result<(Option<PreparedSecurity>, Option<AppContainerSecurity>), WindowsSandboxError> {
    let use_appcontainer = !request.plan.network_access
        || matches!(
            request.plan.mode,
            SandboxFilesystemMode::ReadOnly {
                allow_global_read: false
            }
        );
    let restricted = if use_appcontainer {
        None
    } else {
        Some(prepare_security(request.plan, request.capabilities)?)
    };
    let appcontainer = if use_appcontainer {
        Some(prepare_appcontainer_security(request.plan)?)
    } else {
        None
    };
    Ok((restricted, appcontainer))
}

fn spawn_suspended(
    request: &SandboxSpawnRequest<'_>,
    restricted: Option<&PreparedSecurity>,
    startup: &STARTUPINFOEXW,
    inherit_handles: bool,
    use_pseudo_console: bool,
) -> Result<SandboxedProcess, WindowsSandboxError> {
    let mut command_line = command_line(request.program, request.args);
    let mut environment = environment_block(request.env);
    let cwd = wide_path(request.cwd);
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let mut flags = CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT;
    if !use_pseudo_console {
        flags |= CREATE_NO_WINDOW;
    }
    let created = if let Some(restricted) = restricted {
        unsafe {
            CreateProcessAsUserW(
                restricted.token_handle(),
                std::ptr::null(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                i32::from(inherit_handles),
                flags,
                environment.as_mut_ptr().cast(),
                cwd.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            )
        }
    } else {
        unsafe {
            CreateProcessW(
                std::ptr::null(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                i32::from(inherit_handles),
                flags,
                environment.as_mut_ptr().cast(),
                cwd.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            )
        }
    };
    if created == 0 {
        let operation = if restricted.is_some() {
            "CreateProcessAsUserW"
        } else {
            "CreateProcessW(AppContainer)"
        };
        return Err(win32(operation, unsafe { GetLastError() }));
    }

    let process = OwnedHandle::new(info.hProcess);
    let thread = OwnedHandle::new(info.hThread);
    let job = match ProcessJob::attach(info.dwProcessId) {
        Ok(job) => job,
        Err(error) => {
            unsafe { TerminateProcess(process.raw(), 127) };
            return Err(error.into());
        }
    };
    if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
        let error = io::Error::last_os_error();
        let _ = job.terminate(127);
        return Err(error.into());
    }
    Ok(SandboxedProcess {
        process,
        job: Some(job),
        pid: info.dwProcessId,
    })
}

struct PtyPipeSet {
    console: PseudoConsole,
    input: std::fs::File,
    output: std::fs::File,
}

impl PtyPipeSet {
    fn new(cols: Option<u16>, rows: Option<u16>) -> io::Result<Self> {
        let (console_input, parent_input) = create_pipe_pair()?;
        let (parent_output, console_output) = match create_pipe_pair() {
            Ok(handles) => handles,
            Err(error) => {
                close_handles([console_input, parent_input]);
                return Err(error);
            }
        };
        for handle in [parent_input, parent_output] {
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT as HANDLE_FLAGS, 0) } == 0
            {
                let error = io::Error::last_os_error();
                close_handles([console_input, parent_input, parent_output, console_output]);
                return Err(error);
            }
        }
        let console_input = OwnedHandle::new(console_input);
        let console_output = OwnedHandle::new(console_output);
        let console = match PseudoConsole::new(console_input, console_output, cols, rows) {
            Ok(console) => console,
            Err(error) => {
                close_handles([parent_input, parent_output]);
                return Err(error);
            }
        };
        Ok(Self {
            console,
            input: unsafe { std::fs::File::from_raw_handle(parent_input) },
            output: unsafe { std::fs::File::from_raw_handle(parent_output) },
        })
    }

    fn into_io(self) -> (SandboxedPtyInput, std::fs::File) {
        (
            SandboxedPtyInput {
                console: Some(self.console),
                writer: Some(self.input),
            },
            self.output,
        )
    }
}

fn create_pipe_pair() -> io::Result<(HANDLE, HANDLE)> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((read, write))
    }
}

fn close_handles(handles: impl IntoIterator<Item = HANDLE>) {
    for handle in handles {
        if !handle.is_null() {
            unsafe { CloseHandle(handle) };
        }
    }
}

struct PseudoConsole {
    raw: HPCON,
    api: ConPtyApi,
    _input: OwnedHandle,
    _output: OwnedHandle,
}

// ConPTY handles are kernel-owned resources, and this value is transferred
// between the shell session manager and its cleanup thread while remaining
// exclusively owned by the Rust wrapper.
unsafe impl Send for PseudoConsole {}

impl PseudoConsole {
    fn new(
        input: OwnedHandle,
        output: OwnedHandle,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> io::Result<Self> {
        let api = conpty_api()?;
        let mut raw = 0;
        let result =
            unsafe { (api.create)(pty_size(cols, rows), input.raw(), output.raw(), 0, &mut raw) };
        if result < 0 {
            return Err(io::Error::other(format!(
                "CreatePseudoConsole failed with HRESULT {result:#x}"
            )));
        }
        Ok(Self {
            raw,
            api,
            _input: input,
            _output: output,
        })
    }

    fn raw(&self) -> HPCON {
        self.raw
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let result = unsafe { (self.api.resize)(self.raw, pty_size(Some(cols), Some(rows))) };
        if result < 0 {
            Err(io::Error::other(format!(
                "ResizePseudoConsole failed with HRESULT {result:#x}"
            )))
        } else {
            Ok(())
        }
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        unsafe { (self.api.close)(self.raw) };
    }
}

#[derive(Clone, Copy)]
struct ConPtyApi {
    create: CreatePseudoConsoleFn,
    resize: ResizePseudoConsoleFn,
    close: ClosePseudoConsoleFn,
}

type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> i32;
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);

const KERNEL32_DLL: &[u16] = &[
    b'k' as u16,
    b'e' as u16,
    b'r' as u16,
    b'n' as u16,
    b'e' as u16,
    b'l' as u16,
    b'3' as u16,
    b'2' as u16,
    b'.' as u16,
    b'd' as u16,
    b'l' as u16,
    b'l' as u16,
    0,
];

fn conpty_api() -> io::Result<ConPtyApi> {
    let module = unsafe { GetModuleHandleW(KERNEL32_DLL.as_ptr()) };
    if module.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kernel32.dll is unavailable",
        ));
    }
    let create = conpty_proc(module, b"CreatePseudoConsole\0")?;
    let resize = conpty_proc(module, b"ResizePseudoConsole\0")?;
    let close = conpty_proc(module, b"ClosePseudoConsole\0")?;
    Ok(ConPtyApi {
        create: unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, CreatePseudoConsoleFn>(
                create,
            )
        },
        resize: unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, ResizePseudoConsoleFn>(
                resize,
            )
        },
        close: unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, ClosePseudoConsoleFn>(close)
        },
    })
}

fn conpty_proc(
    module: windows_sys::Win32::Foundation::HMODULE,
    name: &'static [u8],
) -> io::Result<unsafe extern "system" fn() -> isize> {
    unsafe { GetProcAddress(module, name.as_ptr()) }.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "ConPTY requires Windows 10 version 1809 or newer",
        )
    })
}

fn pty_size(cols: Option<u16>, rows: Option<u16>) -> COORD {
    COORD {
        X: cols.unwrap_or(80).clamp(1, i16::MAX as u16) as i16,
        Y: rows.unwrap_or(24).clamp(1, i16::MAX as u16) as i16,
    }
}

struct PipeSet {
    child_stdin: HANDLE,
    child_stdout: HANDLE,
    child_stderr: HANDLE,
    parent_stdin: Option<std::fs::File>,
    parent_stdout: Option<std::fs::File>,
    parent_stderr: Option<std::fs::File>,
}

struct ProcessAttributeList {
    buffer: Vec<u8>,
    handles: Vec<HANDLE>,
    capabilities: Vec<SID_AND_ATTRIBUTES>,
    security_capabilities: Option<Box<SECURITY_CAPABILITIES>>,
    pseudo_console: Option<HPCON>,
}

impl ProcessAttributeList {
    fn new(attribute_count: u32) -> io::Result<Self> {
        let mut size = 0usize;
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), attribute_count, 0, &mut size);
        }
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; size];
        let list = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if unsafe { InitializeProcThreadAttributeList(list, attribute_count, 0, &mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            buffer,
            handles: Vec::new(),
            capabilities: Vec::new(),
            security_capabilities: None,
            pseudo_console: None,
        })
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }

    fn set_handle_list(&mut self, handles: Vec<HANDLE>) -> io::Result<()> {
        self.handles = handles;
        let value = self.handles.as_ptr().cast();
        let size = std::mem::size_of_val(self.handles.as_slice());
        if unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_security_capabilities(
        &mut self,
        app_sid: *mut std::ffi::c_void,
        capability_sids: Vec<*mut std::ffi::c_void>,
    ) -> io::Result<()> {
        self.capabilities = capability_sids
            .into_iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: 0,
            })
            .collect();
        self.security_capabilities = Some(Box::new(SECURITY_CAPABILITIES {
            AppContainerSid: app_sid,
            Capabilities: self.capabilities.as_mut_ptr(),
            CapabilityCount: self.capabilities.len() as u32,
            Reserved: 0,
        }));
        let value = self
            .security_capabilities
            .as_ref()
            .map(|capabilities| (&**capabilities as *const SECURITY_CAPABILITIES).cast())
            .expect("security capabilities");
        let list = self.as_mut_ptr();
        if unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES
                    as usize,
                value,
                std::mem::size_of::<SECURITY_CAPABILITIES>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_pseudo_console(&mut self, pseudo_console: HPCON) -> io::Result<()> {
        self.pseudo_console = Some(pseudo_console);
        let value = self
            .pseudo_console
            .as_ref()
            .map(|handle| (handle as *const HPCON).cast())
            .expect("pseudo console handle");
        if unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                value,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for ProcessAttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

impl PipeSet {
    fn new() -> io::Result<Self> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let mut child_stdin = std::ptr::null_mut();
        let mut parent_stdin = std::ptr::null_mut();
        let mut parent_stdout = std::ptr::null_mut();
        let mut child_stdout = std::ptr::null_mut();
        let mut child_stderr = std::ptr::null_mut();
        let mut parent_stderr = std::ptr::null_mut();
        for (read, write) in [
            (&mut child_stdin, &mut parent_stdin),
            (&mut parent_stdout, &mut child_stdout),
            (&mut parent_stderr, &mut child_stderr),
        ] {
            if unsafe { CreatePipe(read, write, &attributes, 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        for handle in [parent_stdin, parent_stdout, parent_stderr] {
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT as HANDLE_FLAGS, 0) } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            child_stdin,
            child_stdout,
            child_stderr,
            parent_stdin: Some(unsafe { std::fs::File::from_raw_handle(parent_stdin) }),
            parent_stdout: Some(unsafe { std::fs::File::from_raw_handle(parent_stdout) }),
            parent_stderr: Some(unsafe { std::fs::File::from_raw_handle(parent_stderr) }),
        })
    }

    fn close_child_ends(&mut self) {
        for handle in [self.child_stdin, self.child_stdout, self.child_stderr] {
            unsafe { CloseHandle(handle) };
        }
        self.child_stdin = std::ptr::null_mut();
        self.child_stdout = std::ptr::null_mut();
        self.child_stderr = std::ptr::null_mut();
    }
}

impl Drop for PipeSet {
    fn drop(&mut self) {
        self.close_child_ends();
    }
}

fn environment_block(overrides: &BTreeMap<String, Option<String>>) -> Vec<u16> {
    let mut values = std::env::vars_os().collect::<Vec<_>>();
    values.retain(|(key, _)| !key.eq_ignore_ascii_case("ORCA_API_KEY"));
    for (key, value) in overrides {
        values.retain(|(existing, _)| !existing.to_string_lossy().eq_ignore_ascii_case(key));
        if let Some(value) = value {
            values.push((OsString::from(key), OsString::from(value)));
        }
    }
    values.sort_by(|a, b| {
        a.0.to_string_lossy()
            .to_ascii_lowercase()
            .cmp(&b.0.to_string_lossy().to_ascii_lowercase())
    });
    let mut block = Vec::new();
    for (key, value) in values {
        block.extend(key.encode_wide());
        block.push('=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn command_line(program: &Path, args: &[OsString]) -> Vec<u16> {
    let mut line = quote_windows_arg(&program.to_string_lossy());
    for arg in args {
        line.push(' ');
        line.push_str(&quote_windows_arg(&arg.to_string_lossy()));
    }
    line.encode_utf16().chain(std::iter::once(0)).collect()
}

fn quote_windows_arg(value: &str) -> String {
    if !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c == '"') {
        return value.to_string();
    }
    let mut result = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        if character == '\\' {
            slashes += 1;
        } else if character == '"' {
            result.push_str(&"\\".repeat(slashes * 2 + 1));
            result.push(character);
            slashes = 0;
        } else {
            result.push_str(&"\\".repeat(slashes));
            result.push(character);
            slashes = 0;
        }
    }
    result.push_str(&"\\".repeat(slashes * 2));
    result.push('"');
    result
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn exit_status(code: u32) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(code)
}

fn win32(operation: &str, code: u32) -> WindowsSandboxError {
    WindowsSandboxError::Io(io::Error::new(
        io::Error::from_raw_os_error(code as i32).kind(),
        format!("{operation}: {}", io::Error::from_raw_os_error(code as i32)),
    ))
}

struct OwnedHandle {
    raw: HANDLE,
}

// HANDLE ownership is exclusive and the Win32 handle APIs are thread-safe;
// moving the owner between threads does not duplicate or alias the handle.
unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    fn new(raw: HANDLE) -> Self {
        Self { raw }
    }

    fn raw(&self) -> HANDLE {
        self.raw
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { CloseHandle(self.raw) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SandboxFilesystemMode, WindowsSandboxPolicyInput};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn windows_argument_quoting_preserves_backslashes() {
        assert_eq!(quote_windows_arg("plain"), "plain");
        assert_eq!(
            quote_windows_arg(r"C:\Program Files\orca"),
            r#""C:\Program Files\orca""#
        );
        assert_eq!(quote_windows_arg("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn restricted_pipe_child_runs_with_native_token() {
        run_pipe_child(true, true);
    }

    #[test]
    fn restricted_pty_child_runs_with_native_token_and_resizes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let plan = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
            mode: SandboxFilesystemMode::WorkspaceWrite,
            cwd: temp.path().to_path_buf(),
            readable_roots: Vec::new(),
            writable_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_access: true,
        })
        .expect("sandbox plan");
        let capabilities = CapabilityStore::new(temp.path().join("capabilities"));
        let program = PathBuf::from(std::env::var_os("COMSPEC").expect("COMSPEC"));
        let args = vec![
            OsString::from("/D"),
            OsString::from("/S"),
            OsString::from("/C"),
            OsString::from("echo restricted-conpty-ok"),
        ];
        let mut child = SandboxedPty::spawn(
            SandboxSpawnRequest {
                program: &program,
                args: &args,
                cwd: temp.path(),
                env: &BTreeMap::new(),
                plan: &plan,
                capabilities: &capabilities,
            },
            Some(100),
            Some(30),
        )
        .expect("restricted ConPTY child");
        let (mut input, output) = child.take_pty().expect("pty transport");
        let (output_bytes, reader) = output_reader(output);
        input.close();
        input.resize(120, 40).expect("resize after closing stdin");
        let status = child.wait().expect("wait");
        wait_for_output_quiet(&output_bytes, "restricted ConPTY child");
        input.close_terminal();
        let text = reader.join().expect("join ConPTY output reader");
        assert!(status.success(), "{text}");
        assert!(text.contains("restricted-conpty-ok"), "{text:?}");
    }

    #[test]
    fn appcontainer_pipe_child_runs_without_network_capability() {
        run_pipe_child(false, false);
    }

    #[test]
    fn appcontainer_workspace_write_can_write_granted_workspace() {
        run_pipe_child(false, true);
    }

    #[test]
    fn appcontainer_pty_child_runs_and_resizes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let plan = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
            mode: SandboxFilesystemMode::ReadOnly {
                allow_global_read: false,
            },
            cwd: temp.path().to_path_buf(),
            readable_roots: Vec::new(),
            writable_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_access: false,
        })
        .expect("sandbox plan");
        let capabilities = CapabilityStore::new(temp.path().join("capabilities"));
        let program = PathBuf::from(std::env::var_os("COMSPEC").expect("COMSPEC"));
        let args = vec![
            OsString::from("/D"),
            OsString::from("/S"),
            OsString::from("/C"),
            OsString::from("echo appcontainer-conpty-ok"),
        ];
        let mut child = SandboxedPty::spawn(
            SandboxSpawnRequest {
                program: &program,
                args: &args,
                cwd: temp.path(),
                env: &BTreeMap::new(),
                plan: &plan,
                capabilities: &capabilities,
            },
            Some(100),
            Some(30),
        )
        .expect("AppContainer ConPTY child");
        let (mut input, output) = child.take_pty().expect("pty transport");
        let (output_bytes, reader) = output_reader(output);
        input.close();
        input.resize(120, 40).expect("resize after closing stdin");
        let status = child.wait().expect("wait");
        wait_for_output_quiet(&output_bytes, "AppContainer ConPTY child");
        input.close_terminal();
        let text = reader.join().expect("join ConPTY output reader");
        assert!(status.success(), "{text}");
        assert!(text.contains("appcontainer-conpty-ok"), "{text:?}");
    }

    #[test]
    fn appcontainer_without_network_capability_cannot_connect_to_loopback() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("listener address").port();
        let temp = tempfile::tempdir().expect("tempdir");
        let plan = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
            mode: SandboxFilesystemMode::ReadOnly {
                allow_global_read: false,
            },
            cwd: temp.path().to_path_buf(),
            readable_roots: Vec::new(),
            writable_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_access: false,
        })
        .expect("sandbox plan");
        let capabilities = CapabilityStore::new(temp.path().join("capabilities"));
        let program = powershell_program();
        let script = format!(
            "$client = New-Object Net.Sockets.TcpClient; $client.ConnectAsync('127.0.0.1', {port}).Wait(1000); if ($client.Connected) {{ exit 0 }} else {{ exit 1 }}"
        );
        let args = vec![
            OsString::from("-NoLogo"),
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-Command"),
            OsString::from(script),
        ];
        let mut child = SandboxedChild::spawn(SandboxSpawnRequest {
            program: &program,
            args: &args,
            cwd: temp.path(),
            env: &BTreeMap::new(),
            plan: &plan,
            capabilities: &capabilities,
        })
        .expect("AppContainer child");
        let (_stdin, mut stdout, mut stderr) = child.take_stdio().expect("stdio");
        let mut output = String::new();
        stdout.read_to_string(&mut output).expect("stdout");
        let mut error = String::new();
        stderr.read_to_string(&mut error).expect("stderr");
        let status = child.wait().expect("wait");
        assert!(
            !status.success(),
            "network-disabled AppContainer reached loopback: stdout={output:?}, stderr={error:?}"
        );
    }

    #[test]
    fn appcontainer_strict_read_cannot_read_ungranted_sibling_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let secret = temp.path().join("host-secret.txt");
        std::fs::write(&secret, "must remain private").expect("secret");
        let plan = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
            mode: SandboxFilesystemMode::ReadOnly {
                allow_global_read: false,
            },
            cwd: workspace.clone(),
            readable_roots: Vec::new(),
            writable_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_access: true,
        })
        .expect("sandbox plan");
        let capabilities = CapabilityStore::new(temp.path().join("capabilities"));
        let mut env = BTreeMap::new();
        env.insert(
            "ORCA_TEST_UNGRANTED_FILE".to_string(),
            Some(secret.display().to_string()),
        );
        let args = vec![
            OsString::from("-NoLogo"),
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-Command"),
            OsString::from(
                "Get-Content -LiteralPath $env:ORCA_TEST_UNGRANTED_FILE -ErrorAction Stop | Out-Null",
            ),
        ];
        let mut child = SandboxedChild::spawn(SandboxSpawnRequest {
            program: &powershell_program(),
            args: &args,
            cwd: &workspace,
            env: &env,
            plan: &plan,
            capabilities: &capabilities,
        })
        .expect("AppContainer child");
        let (_stdin, mut stdout, mut stderr) = child.take_stdio().expect("stdio");
        let mut output = String::new();
        stdout.read_to_string(&mut output).expect("stdout");
        let mut error = String::new();
        stderr.read_to_string(&mut error).expect("stderr");
        let status = child.wait().expect("wait");
        assert!(
            !status.success(),
            "strict-read AppContainer accessed an ungranted file: stdout={output:?}, stderr={error:?}"
        );
    }

    fn run_pipe_child(network_access: bool, allow_global_read: bool) {
        let temp = tempfile::tempdir().expect("tempdir");
        let plan = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
            mode: if allow_global_read {
                SandboxFilesystemMode::WorkspaceWrite
            } else {
                SandboxFilesystemMode::ReadOnly {
                    allow_global_read: false,
                }
            },
            cwd: temp.path().to_path_buf(),
            readable_roots: Vec::new(),
            writable_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_access,
        })
        .expect("sandbox plan");
        let capabilities = CapabilityStore::new(temp.path().join("capabilities"));
        let program = PathBuf::from(std::env::var_os("COMSPEC").expect("COMSPEC"));
        let script = if allow_global_read {
            "echo sandbox-ok > sandbox-output.txt && type sandbox-output.txt"
        } else {
            "echo sandbox-ok"
        };
        let args = vec![
            OsString::from("/D"),
            OsString::from("/S"),
            OsString::from("/C"),
            OsString::from(script),
        ];
        let mut child = SandboxedChild::spawn(SandboxSpawnRequest {
            program: &program,
            args: &args,
            cwd: temp.path(),
            env: &BTreeMap::new(),
            plan: &plan,
            capabilities: &capabilities,
        })
        .expect("restricted child");
        let (_stdin, mut stdout, mut stderr) = child.take_stdio().expect("stdio");
        let mut output = String::new();
        stdout.read_to_string(&mut output).expect("stdout");
        let mut error = String::new();
        stderr.read_to_string(&mut error).expect("stderr");
        let status = child.wait().expect("wait");
        assert!(status.success(), "{error}");
        assert!(output.contains("sandbox-ok"), "{output:?}");
        if allow_global_read {
            assert_eq!(
                std::fs::read_to_string(temp.path().join("sandbox-output.txt"))
                    .expect("granted workspace output")
                    .trim(),
                "sandbox-ok"
            );
        }
    }

    fn powershell_program() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe")
    }

    fn output_reader(
        mut output: Box<dyn Read + Send>,
    ) -> (Arc<AtomicUsize>, std::thread::JoinHandle<String>) {
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&bytes_read);
        let handle = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                match output.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        bytes.extend_from_slice(&buffer[..count]);
                        observed.fetch_add(count, Ordering::Release);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => panic!("read sandbox ConPTY output: {error}"),
                }
            }
            String::from_utf8_lossy(&bytes).into_owned()
        });
        (bytes_read, handle)
    }

    fn wait_for_output_quiet(bytes_read: &AtomicUsize, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut observed = 0;
        let mut quiet_since = Instant::now();
        loop {
            let current = bytes_read.load(Ordering::Acquire);
            if current != observed {
                observed = current;
                quiet_since = Instant::now();
            }
            if observed > 0 && quiet_since.elapsed() >= Duration::from_millis(200) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{label} produced no ConPTY output before the drain deadline"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
