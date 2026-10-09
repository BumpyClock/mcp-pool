use std::io;

use tokio::process::{Child, Command};

pub(super) struct Ownership {
    process_group: Option<libc::pid_t>,
}

impl Ownership {
    /// On Linux, adopts orphaned descendants so they can be reaped with this group.
    pub(super) fn prepare(command: &mut Command) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        command.process_group(0);
        Ok(Self {
            process_group: None,
        })
    }

    pub(super) fn attach(mut self, child: &Child) -> io::Result<Self> {
        let process_id = child
            .id()
            .ok_or_else(|| io::Error::other("missing upstream process id"))?;
        self.process_group = Some(
            libc::pid_t::try_from(process_id)
                .map_err(|_| io::Error::other("upstream process id exceeds platform range"))?,
        );
        Ok(self)
    }

    pub(super) fn terminate(&mut self) -> io::Result<()> {
        self.signal(libc::SIGKILL).map(|_| ())
    }

    pub(super) fn is_empty(&self) -> io::Result<bool> {
        #[cfg(target_os = "linux")]
        self.reap_adopted_descendants()?;
        self.signal(0).map(|exists| !exists)
    }

    #[cfg(target_os = "linux")]
    /// Reaps only descendants in this owned group, never unrelated daemon children.
    fn reap_adopted_descendants(&self) -> io::Result<()> {
        let process_group = self
            .process_group
            .ok_or_else(|| io::Error::other("upstream process group was not established"))?;
        loop {
            match unsafe { libc::waitpid(-process_group, std::ptr::null_mut(), libc::WNOHANG) } {
                0 => return Ok(()),
                value if value > 0 => {}
                _ => {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ECHILD) {
                        return Ok(());
                    }
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        }
    }

    pub(super) fn disarm(&mut self) {
        self.process_group = None;
    }

    /// Observes leader exit without reaping it, keeping its PID valid for group signaling.
    pub(super) async fn wait_for_exit(&self) -> io::Result<()> {
        let process_group = self
            .process_group
            .ok_or_else(|| io::Error::other("upstream process group was not established"))?;
        loop {
            let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    process_group as libc::id_t,
                    &mut information,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            } != 0
            {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            } else if unsafe { information.si_pid() } != 0 {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Signals only the process group created for this child.
    fn signal(&self, signal: libc::c_int) -> io::Result<bool> {
        let process_group = self
            .process_group
            .ok_or_else(|| io::Error::other("upstream process group was not established"))?;
        if unsafe { libc::killpg(process_group, signal) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

impl Drop for Ownership {
    fn drop(&mut self) {
        if self.process_group.is_some()
            && let Err(error) = self.terminate()
        {
            crate::diagnostics::log(format!("upstream_process_group_drop_error error={error}"));
        }
    }
}
