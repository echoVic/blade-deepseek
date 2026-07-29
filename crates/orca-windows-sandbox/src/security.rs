use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use orca_platform::fs::PathPolicy;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, GetLastError, HANDLE, HLOCAL, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetSecurityInfo, SET_ACCESS, SetEntriesInAclW,
    SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    ACL, AdjustTokenPrivileges, CopySid, CreateRestrictedToken, CreateWellKnownSid,
    DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE, FreeSid, GetLengthSid, GetTokenInformation,
    LUA_TOKEN, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, SID_AND_ATTRIBUTES,
    SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES, TOKEN_ADJUST_SESSIONID,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_INFORMATION_CLASS, TOKEN_PRIVILEGES, TOKEN_QUERY,
    TokenDefaultDacl, TokenGroups, WRITE_RESTRICTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::{CapabilityStore, SandboxFilesystemMode, WindowsSandboxError, WindowsSandboxPlan};

const SE_FILE_OBJECT: i32 = 1;
const WIN_WORLD_SID: i32 = 1;
const CONTAINER_INHERIT_ACE: u32 = 0x02;
const OBJECT_INHERIT_ACE: u32 = 0x01;
const TOKEN_DEFAULT_DACL_CLASS: TOKEN_INFORMATION_CLASS = TokenDefaultDacl;
const SE_GROUP_LOGON_ID: u32 = 0xc000_0000;

pub struct PreparedSecurity {
    token: OwnedHandle,
}

pub struct AppContainerSecurity {
    app_sid: LocalSid,
    capability_sids: Vec<LocalSid>,
}

impl PreparedSecurity {
    pub(super) fn token_handle(&self) -> HANDLE {
        self.token.raw()
    }
}

impl AppContainerSecurity {
    pub(super) fn app_sid(&self) -> *mut c_void {
        self.app_sid.as_ptr()
    }

    pub(super) fn capability_sids(&self) -> Vec<*mut c_void> {
        self.capability_sids.iter().map(LocalSid::as_ptr).collect()
    }
}

pub fn prepare_security(
    plan: &WindowsSandboxPlan,
    capabilities: &CapabilityStore,
) -> Result<PreparedSecurity, WindowsSandboxError> {
    if !plan.network_access {
        return Err(WindowsSandboxError::InvalidPolicy(
            "Windows capability tokens do not enforce network isolation; use the native WFP/AppContainer backend"
                .to_string(),
        ));
    }
    if matches!(
        plan.mode,
        SandboxFilesystemMode::ReadOnly {
            allow_global_read: false
        }
    ) {
        return Err(WindowsSandboxError::InvalidPolicy(
            "restricted Windows reads require the native AppContainer/elevated backend".to_string(),
        ));
    }

    let mut sid_strings = Vec::new();
    let mut sid_roots = Vec::new();
    if matches!(plan.mode, SandboxFilesystemMode::ReadOnly { .. }) {
        sid_strings.push(capabilities.read_only_sid()?);
    }
    for root in &plan.writable_roots {
        let sid = capabilities.write_sid(root)?;
        if !sid_strings.contains(&sid) {
            sid_strings.push(sid.clone());
        }
        sid_roots.push((root, sid));
    }
    if sid_strings.is_empty() {
        return Err(WindowsSandboxError::InvalidPolicy(
            "Windows sandbox requires at least one capability SID".to_string(),
        ));
    }

    let sids = sid_strings
        .iter()
        .map(|sid| LocalSid::from_string(sid))
        .collect::<Result<Vec<_>, _>>()?;
    let sid_ptrs = sids.iter().map(LocalSid::as_ptr).collect::<Vec<_>>();

    let writable_sids = sid_roots
        .iter()
        .map(|(_, sid)| LocalSid::from_string(sid))
        .collect::<Result<Vec<_>, _>>()?;
    let writable_acl_sids = sid_roots
        .iter()
        .zip(&writable_sids)
        .map(|((root, _), sid)| (root.as_path(), sid.as_ptr()))
        .collect::<Vec<_>>();
    apply_plan_acls(plan, &sid_ptrs, &writable_acl_sids)?;

    let token = create_restricted_token(&sid_ptrs)?;
    Ok(PreparedSecurity { token })
}

pub fn prepare_appcontainer_security(
    plan: &WindowsSandboxPlan,
) -> Result<AppContainerSecurity, WindowsSandboxError> {
    let app_sid = appcontainer_sid()?;
    let capability_sids = if plan.network_access {
        vec![LocalSid::from_string("S-1-15-3-1")?]
    } else {
        Vec::new()
    };
    let app_sid_ptr = app_sid.as_ptr();
    let writable_acl_sids = plan
        .writable_roots
        .iter()
        .map(|root| (root.as_path(), app_sid_ptr))
        .collect::<Vec<_>>();
    apply_plan_acls(plan, &[app_sid_ptr], &writable_acl_sids)?;
    Ok(AppContainerSecurity {
        app_sid,
        capability_sids,
    })
}

/// Ensure the stable AppContainer identity exists before the runtime needs to
/// apply a policy-specific ACL. ACLs remain scoped to each spawn plan.
pub fn ensure_appcontainer_profile() -> Result<(), WindowsSandboxError> {
    appcontainer_sid().map(|_| ())
}

fn apply_plan_acls(
    plan: &WindowsSandboxPlan,
    readable_sids: &[*mut c_void],
    writable_sids: &[(&Path, *mut c_void)],
) -> Result<(), WindowsSandboxError> {
    for root in &plan.readable_roots {
        for sid in readable_sids {
            ensure_acl_entry(
                root,
                *sid,
                FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
                SET_ACCESS,
                true,
            )?;
        }
    }
    for (root, sid) in writable_sids {
        ensure_acl_entry(
            root,
            *sid,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
            SET_ACCESS,
            true,
        )?;
    }
    for denied in &plan.denied_roots {
        if !denied.exists() {
            if is_optional_metadata_root(denied) {
                continue;
            }
            return Err(WindowsSandboxError::InvalidPolicy(format!(
                "denied root does not exist: {}",
                denied.display()
            )));
        }
        for sid in readable_sids {
            ensure_acl_entry(
                denied,
                *sid,
                FILE_GENERIC_WRITE | DELETE | FILE_DELETE_CHILD,
                DENY_ACCESS,
                true,
            )?;
        }
    }
    Ok(())
}

fn appcontainer_sid() -> Result<LocalSid, WindowsSandboxError> {
    static PROFILE_REGISTRATION: OnceLock<Mutex<()>> = OnceLock::new();
    let _registration = PROFILE_REGISTRATION
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    const PROFILE_NAME: &str = "Orca.WindowsSandbox.v1";
    const ERROR_ALREADY_EXISTS_HRESULT: i32 = 0x8007_00b7_u32 as i32;
    let name = wide(PROFILE_NAME);
    let mut sid = std::ptr::null_mut();
    let created = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            name.as_ptr(),
            name.as_ptr(),
            std::ptr::null(),
            0,
            &mut sid,
        )
    };
    if created >= 0 && !sid.is_null() {
        return Ok(LocalSid::from_appcontainer(sid));
    }
    if created != ERROR_ALREADY_EXISTS_HRESULT {
        if !sid.is_null() {
            unsafe { FreeSid(sid) };
        }
        return Err(WindowsSandboxError::Io(io::Error::other(format!(
            "CreateAppContainerProfile failed with HRESULT {created:#x}"
        ))));
    }

    if !sid.is_null() {
        unsafe { FreeSid(sid) };
    }
    sid = std::ptr::null_mut();
    let derived = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
    if derived >= 0 && !sid.is_null() {
        Ok(LocalSid::from_appcontainer(sid))
    } else {
        Err(WindowsSandboxError::Io(io::Error::other(format!(
            "DeriveAppContainerSidFromAppContainerName failed with HRESULT {derived:#x}"
        ))))
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn is_optional_metadata_root(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                ".git" | ".agents" | ".codex"
            )
        })
}

fn ensure_acl_entry(
    path: &Path,
    sid: *mut c_void,
    permissions: u32,
    mode: i32,
    inherited: bool,
) -> Result<(), WindowsSandboxError> {
    let verified = PathPolicy::windows_sandbox()
        .open_no_follow_with_access(path, FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC)?;
    let handle = verified.file().as_raw_handle() as HANDLE;
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut security_descriptor: *mut c_void = std::ptr::null_mut();
    let code = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut old_dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(win32("GetSecurityInfo", code));
    }

    let trustee = EXPLICIT_ACCESS_W {
        grfAccessPermissions: permissions,
        grfAccessMode: mode,
        grfInheritance: if inherited {
            CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
        } else {
            0
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        },
    };
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let code = unsafe { SetEntriesInAclW(1, &trustee, old_dacl, &mut new_dacl) };
    if code != ERROR_SUCCESS {
        free_local(security_descriptor);
        return Err(win32("SetEntriesInAclW", code));
    }
    let code = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    free_local(new_dacl as *mut c_void);
    free_local(security_descriptor);
    if code != ERROR_SUCCESS {
        return Err(win32("SetSecurityInfo", code));
    }
    Ok(())
}

fn create_restricted_token(sids: &[*mut c_void]) -> Result<OwnedHandle, WindowsSandboxError> {
    let base = open_current_token()?;
    let mut logon_sid = logon_sid(base.raw())?;
    let mut entries = sids
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: *sid,
            Attributes: 0,
        })
        .collect::<Vec<_>>();
    let mut world = world_sid()?;
    entries.push(SID_AND_ATTRIBUTES {
        Sid: world.as_mut_ptr() as *mut c_void,
        Attributes: 0,
    });
    entries.push(SID_AND_ATTRIBUTES {
        Sid: logon_sid.as_mut_ptr() as *mut c_void,
        Attributes: 0,
    });

    let flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED;
    let mut token: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        CreateRestrictedToken(
            base.raw(),
            flags,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            entries.len() as u32,
            entries.as_ptr(),
            &mut token,
        )
    };
    if ok == 0 {
        return Err(win32("CreateRestrictedToken", unsafe { GetLastError() }));
    }
    let restricted = OwnedHandle::new(token);
    let mut default_sids = sids.to_vec();
    default_sids.push(world.as_mut_ptr() as *mut c_void);
    default_sids.push(logon_sid.as_mut_ptr() as *mut c_void);
    set_default_dacl(restricted.raw(), &default_sids)?;
    enable_change_notify(restricted.raw())?;
    Ok(restricted)
}

fn logon_sid(token: HANDLE) -> Result<Vec<u8>, WindowsSandboxError> {
    let mut size = 0u32;
    unsafe {
        GetTokenInformation(token, TokenGroups, std::ptr::null_mut(), 0, &mut size);
    }
    if size == 0 {
        return Err(win32("GetTokenInformation(TokenGroups)", unsafe {
            GetLastError()
        }));
    }
    let mut groups = vec![0u8; size as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            groups.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(win32("GetTokenInformation(TokenGroups)", unsafe {
            GetLastError()
        }));
    }

    let count = unsafe { std::ptr::read_unaligned(groups.as_ptr().cast::<u32>()) } as usize;
    let after_count = unsafe { groups.as_ptr().add(std::mem::size_of::<u32>()) } as usize;
    let alignment = std::mem::align_of::<SID_AND_ATTRIBUTES>();
    let entries = ((after_count + alignment - 1) & !(alignment - 1)) as *const SID_AND_ATTRIBUTES;
    for index in 0..count {
        let entry = unsafe { std::ptr::read_unaligned(entries.add(index)) };
        if entry.Attributes & SE_GROUP_LOGON_ID != SE_GROUP_LOGON_ID {
            continue;
        }
        let sid_size = unsafe { GetLengthSid(entry.Sid) };
        if sid_size == 0 {
            return Err(win32("GetLengthSid(logon SID)", unsafe { GetLastError() }));
        }
        let mut sid = vec![0u8; sid_size as usize];
        if unsafe { CopySid(sid_size, sid.as_mut_ptr().cast(), entry.Sid) } == 0 {
            return Err(win32("CopySid(logon SID)", unsafe { GetLastError() }));
        }
        return Ok(sid);
    }
    Err(WindowsSandboxError::InvalidPolicy(
        "current Windows token does not expose a logon SID".to_string(),
    ))
}

fn open_current_token() -> Result<OwnedHandle, WindowsSandboxError> {
    let desired = TOKEN_DUPLICATE
        | TOKEN_QUERY
        | TOKEN_ASSIGN_PRIMARY
        | TOKEN_ADJUST_DEFAULT
        | TOKEN_ADJUST_SESSIONID
        | TOKEN_ADJUST_PRIVILEGES;
    let mut token: HANDLE = std::ptr::null_mut();
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), desired, &mut token) };
    if ok == 0 {
        return Err(win32("OpenProcessToken", unsafe { GetLastError() }));
    }
    Ok(OwnedHandle::new(token))
}

fn world_sid() -> Result<Vec<u8>, WindowsSandboxError> {
    let mut size = 0u32;
    unsafe {
        CreateWellKnownSid(
            WIN_WORLD_SID,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );
    }
    let mut sid = vec![0u8; size as usize];
    let ok = unsafe {
        CreateWellKnownSid(
            WIN_WORLD_SID,
            std::ptr::null_mut(),
            sid.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(win32("CreateWellKnownSid", unsafe { GetLastError() }));
    }
    sid.truncate(size as usize);
    Ok(sid)
}

fn set_default_dacl(token: HANDLE, sids: &[*mut c_void]) -> Result<(), WindowsSandboxError> {
    let entries = sids
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: 0x1000_0000,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: *sid as *mut u16,
            },
        })
        .collect::<Vec<_>>();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let code = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null_mut(),
            &mut dacl,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(win32("SetEntriesInAclW default DACL", code));
    }
    #[repr(C)]
    struct TokenDefaultDacl {
        default_dacl: *mut ACL,
    }
    let info = TokenDefaultDacl { default_dacl: dacl };
    let ok = unsafe {
        SetTokenInformation(
            token,
            TOKEN_DEFAULT_DACL_CLASS,
            (&info as *const TokenDefaultDacl).cast(),
            std::mem::size_of::<TokenDefaultDacl>() as u32,
        )
    };
    free_local(dacl as *mut c_void);
    if ok == 0 {
        return Err(win32("SetTokenInformation(TokenDefaultDacl)", unsafe {
            GetLastError()
        }));
    }
    Ok(())
}

fn enable_change_notify(token: HANDLE) -> Result<(), WindowsSandboxError> {
    let mut luid = unsafe { std::mem::zeroed() };
    let name = "SeChangeNotifyPrivilege";
    let name = name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let ok = unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) };
    if ok == 0 {
        return Err(win32("LookupPrivilegeValueW", unsafe { GetLastError() }));
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [windows_sys::Win32::Security::LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    let ok = unsafe {
        AdjustTokenPrivileges(
            token,
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(win32("AdjustTokenPrivileges", unsafe { GetLastError() }));
    }
    Ok(())
}

struct LocalSid {
    raw: *mut c_void,
    allocator: SidAllocator,
}

enum SidAllocator {
    Local,
    AppContainer,
}

impl LocalSid {
    fn from_string(value: &str) -> Result<Self, WindowsSandboxError> {
        let value = value
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut sid = std::ptr::null_mut();
        let ok = unsafe {
            windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW(
                value.as_ptr(),
                &mut sid,
            )
        };
        if ok == 0 || sid.is_null() {
            return Err(win32("ConvertStringSidToSidW", unsafe { GetLastError() }));
        }
        Ok(Self {
            raw: sid,
            allocator: SidAllocator::Local,
        })
    }

    fn from_appcontainer(raw: *mut c_void) -> Self {
        Self {
            raw,
            allocator: SidAllocator::AppContainer,
        }
    }

    fn as_ptr(&self) -> *mut c_void {
        self.raw
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        match self.allocator {
            SidAllocator::Local => free_local(self.raw),
            SidAllocator::AppContainer if !self.raw.is_null() => unsafe {
                FreeSid(self.raw);
            },
            SidAllocator::AppContainer => {}
        }
    }
}

struct OwnedHandle {
    raw: HANDLE,
}

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

fn free_local(raw: *mut c_void) {
    if !raw.is_null() {
        unsafe { LocalFree(raw as HLOCAL) };
    }
}

fn win32(operation: &str, code: u32) -> WindowsSandboxError {
    WindowsSandboxError::Io(io::Error::from_raw_os_error(code as i32)).into_context(operation)
}

trait IoContext {
    fn into_context(self, operation: &str) -> WindowsSandboxError;
}

impl IoContext for WindowsSandboxError {
    fn into_context(self, operation: &str) -> WindowsSandboxError {
        match self {
            Self::Io(error) => WindowsSandboxError::Io(io::Error::new(
                error.kind(),
                format!("{operation}: {error}"),
            )),
            other => other,
        }
    }
}
