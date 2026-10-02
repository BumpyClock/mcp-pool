use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenProcess, OpenThread, ResumeThread,
    SYNCHRONIZATION_SYNCHRONIZE, THREAD_SUSPEND_RESUME, WaitForSingleObject,
};

pub(super) struct Ownership {
    job: OwnedHandle,
    members: Vec<OwnedHandle>,
}

impl Ownership {
    pub(super) fn prepare(command: &mut Command) -> io::Result<Self> {
        // Suspension prevents a launcher from creating descendants before assignment.
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let ownership = Self {
            job: unsafe { OwnedHandle::from_raw_handle(job) },
            members: Vec::new(),
        };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                ownership.handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(ownership)
    }

    pub(super) fn attach(self, child: &Child) -> io::Result<Self> {
        let process_handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("missing upstream process handle"))?;
        if unsafe { AssignProcessToJobObject(self.handle(), process_handle.cast()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let process_id = child
            .id()
            .ok_or_else(|| io::Error::other("missing upstream process id"))?;
        resume_initial_thread(process_id)?;
        Ok(self)
    }

    pub(super) fn terminate(&mut self) -> io::Result<()> {
        self.members = self.member_handles()?;
        if unsafe { TerminateJobObject(self.handle(), 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn is_empty(&self) -> io::Result<bool> {
        let mut information: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        if unsafe {
            QueryInformationJobObject(
                self.handle(),
                JobObjectBasicAccountingInformation,
                (&mut information as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if information.ActiveProcesses != 0 {
            return Ok(false);
        }
        // A zero job count can precede the final process-handle signal.
        for member in &self.members {
            match unsafe { WaitForSingleObject(member.as_raw_handle().cast(), 0) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Ok(false),
                _ => return Err(io::Error::last_os_error()),
            }
        }
        Ok(true)
    }

    pub(super) fn disarm(&mut self) {}

    fn handle(&self) -> HANDLE {
        self.job.as_raw_handle().cast()
    }

    fn member_handles(&self) -> io::Result<Vec<OwnedHandle>> {
        let mut capacity = 64usize;
        loop {
            let bytes =
                size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() + capacity * size_of::<usize>();
            let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
            let information = storage
                .as_mut_ptr()
                .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            if unsafe {
                QueryInformationJobObject(
                    self.handle(),
                    JobObjectBasicProcessIdList,
                    information.cast(),
                    bytes as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_MORE_DATA as i32) && capacity < 65536 {
                    capacity *= 2;
                    continue;
                }
                return Err(error);
            }
            let count = unsafe { (*information).NumberOfProcessIdsInList } as usize;
            if count > capacity {
                return Err(io::Error::other(
                    "upstream job process list exceeded its buffer",
                ));
            }
            // The platform writes count ids into the aligned, oversized buffer.
            let identifiers =
                unsafe { std::slice::from_raw_parts((*information).ProcessIdList.as_ptr(), count) };
            let mut handles = Vec::with_capacity(count);
            for identifier in identifiers {
                let process_id = u32::try_from(*identifier).map_err(io::Error::other)?;
                let handle = unsafe { OpenProcess(SYNCHRONIZATION_SYNCHRONIZE, 0, process_id) };
                if handle.is_null() {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32) {
                        return Err(error);
                    }
                } else {
                    handles.push(unsafe { OwnedHandle::from_raw_handle(handle) });
                }
            }
            return Ok(handles);
        }
    }
}

fn resume_initial_thread(process_id: u32) -> io::Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle().cast(), &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == process_id {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        found = unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &mut entry) };
    }
    Err(io::Error::other(
        "suspended upstream initial thread was not found",
    ))
}
