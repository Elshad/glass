//! Fleet scripts on Windows: who may change a script (its owner and ACL, as Unix modes say
//! on a Mac), and running one in a Job object so a stop takes everything it started.

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
use windows::core::HSTRING;

/// Who may change a file or folder.
#[derive(Debug, PartialEq)]
pub enum Access {
    /// Only this user, SYSTEM and Administrators.
    Private,
    /// Owned by someone else, who could change its permissions.
    NotYours,
    OthersCanWrite,
}

/// A SID in a buffer of its own.
struct Sid(Vec<u64>);

impl Sid {
    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Self> {
        let mut buf = vec![0u64; SECURITY_MAX_SID_SIZE as usize / 8 + 1];
        let mut len = (buf.len() * 8) as u32;
        // SAFETY: buf holds len bytes.
        unsafe { CreateWellKnownSid(kind, None, Some(PSID(buf.as_mut_ptr().cast())), &mut len)? };
        Ok(Self(buf))
    }

    /// This process's user. The SID sits inside the TOKEN_USER the buffer holds.
    fn me() -> Result<(Self, PSID)> {
        // SAFETY: the token handle is closed before returning; the buffer is sized by the
        // first call, u64-aligned, and kept alive alongside the SID pointing into it.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            let mut buf = vec![0u64; len as usize / 8 + 1];
            let got = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), (buf.len() * 8) as u32, &mut len);
            let _ = CloseHandle(token);
            got?;
            let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
            Ok((Self(buf), sid))
        }
    }

    fn psid(&self) -> PSID {
        PSID(self.0.as_ptr() as *mut _)
    }
}

/// Reads `path`'s owner and ACL.
pub fn access(path: &Path) -> Result<Access> {
    let (_me_buf, me) = Sid::me()?;
    let system = Sid::well_known(WinLocalSystemSid)?;
    let admins = Sid::well_known(WinBuiltinAdministratorsSid)?;
    // SAFETY: the SIDs compared all point into live buffers (ours, or the descriptor's).
    let trusted = |sid: PSID| [me, system.psid(), admins.psid()].into_iter().any(|t| unsafe { EqualSid(t, sid) }.is_ok());
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the out pointers are valid; owner and dacl point into sd, freed below.
    let err = unsafe {
        GetNamedSecurityInfoW(
            &HSTRING::from(path),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        )
    };
    ensure!(err == ERROR_SUCCESS, "can't read who may change {} ({err:?})", path.display());
    let verdict = (|| {
        if !trusted(owner) {
            return Access::NotYours;
        }
        // No ACL at all lets everyone do anything.
        if dacl.is_null() {
            return Access::OthersCanWrite;
        }
        // SAFETY: dacl is the descriptor's ACL; GetAce hands back pointers inside it, each
        // starting with an ACE_HEADER, and the allowed kinds checked have the ACCESS_ALLOWED_ACE
        // layout with the SID at SidStart.
        unsafe {
            for i in 0..(*dacl).AceCount as u32 {
                let mut ace = std::ptr::null_mut();
                if GetAce(dacl, i, &mut ace).is_err() {
                    return Access::OthersCanWrite;
                }
                let header = *(ace as *const ACE_HEADER);
                let allowed = ace as *const ACCESS_ALLOWED_ACE;
                let (mask, sid_trusted) = match header.AceType {
                    0 | 9 => ((*allowed).Mask, trusted(PSID(std::ptr::addr_of!((*allowed).SidStart) as *mut _))),
                    _ => (0, false),
                };
                if crate::actions::ace_lets_others_write(header.AceType, header.AceFlags, mask, sid_trusted) {
                    return Access::OthersCanWrite;
                }
            }
        }
        Access::Private
    })();
    // SAFETY: sd was allocated by GetNamedSecurityInfoW for us to free.
    unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    Ok(verdict)
}

/// A Job object: everything in it ends when it's closed.
struct Job(HANDLE);

impl Job {
    fn new() -> std::io::Result<Self> {
        // SAFETY: the handle is owned by the returned Job and closed on drop; info is the
        // struct the information class expects.
        unsafe {
            let job = Self(CreateJobObjectW(None, None)?);
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
            Ok(job)
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: our handle; closing it ends whatever is still in the job.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Resumes a process started suspended (its only thread).
fn resume(pid: u32) -> std::io::Result<()> {
    // SAFETY: the snapshot and thread handles are closed before returning; entry is sized.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)?;
        let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut resumed = false;
        let mut more = Thread32First(snap, &mut entry).is_ok();
        while more {
            if entry.th32OwnerProcessID == pid
                && let Ok(t) = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
            {
                resumed |= ResumeThread(t) != u32::MAX;
                let _ = CloseHandle(t);
            }
            more = Thread32Next(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
        if resumed { Ok(()) } else { Err(std::io::Error::other("couldn't start the script's process")) }
    }
}

/// Runs `cmd` with no window, inside a Job object, for at most `limit`. `None` when it ran
/// on and was stopped; then, or once it's done, whatever it left running is stopped too.
pub fn run(mut cmd: Command, limit: Duration) -> std::io::Result<Option<ExitStatus>> {
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let job = Job::new()?;
    // Suspended until it's in the job, so nothing it starts can slip out first.
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = cmd.spawn()?;
    // SAFETY: the child's process handle stays open while `child` lives.
    let placed = unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) }.map_err(std::io::Error::from).and_then(|_| resume(child.id()));
    if let Err(e) = placed {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    let started = Instant::now();
    loop {
        if let Some(s) = child.try_wait()? {
            return Ok(Some(s));
        }
        if started.elapsed() > limit {
            // SAFETY: our job handle.
            let _ = unsafe { TerminateJobObject(job.0, 1) };
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
