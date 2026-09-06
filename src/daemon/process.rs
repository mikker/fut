/// Verify inherited terminal context against the live process tree. A desktop
/// app can inherit FUT_* while being reparented outside the terminal's tree.
pub(super) fn is_descendant(mut pid: u32, ancestor: u32) -> bool {
    // Bound traversal even if processes disappear or are reparented mid-walk.
    for _ in 0..128 {
        if pid <= 1 {
            return false;
        }
        if pid == ancestor {
            return true;
        }
        let Some(parent) = parent_pid(pid) else {
            return false;
        };
        if parent == pid {
            return false;
        }
        pid = parent;
    }
    false
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: u32) -> Option<u32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: proc_pidinfo writes at most size bytes into this valid buffer.
    let written = unsafe {
        libc::proc_pidinfo(
            pid.try_into().ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled the complete structure.
    Some(unsafe { info.assume_init() }.pbi_ppid)
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces and parentheses; the final ')' ends that field.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_pid(_pid: u32) -> Option<u32> {
    None
}
