/// Verify inherited terminal context against the live process tree. A desktop
/// app can inherit FUT_* while being reparented outside the terminal's tree.
pub(super) fn is_descendant(pid: u32, ancestor: u32) -> bool {
    lineage(pid, ancestor).is_some()
}

/// Return the process and its ancestors through `ancestor`, but only when the
/// complete chain still belongs to that ancestor. Processes may disappear
/// while the chain is inspected, in which case there is no trustworthy owner.
pub(super) fn lineage(mut pid: u32, ancestor: u32) -> Option<Vec<u32>> {
    let mut lineage = Vec::new();
    for _ in 0..128 {
        if pid <= 1 {
            return None;
        }
        lineage.push(pid);
        if pid == ancestor {
            return Some(lineage);
        }
        let parent = parent_pid(pid)?;
        if parent == pid {
            return None;
        }
        pid = parent;
    }
    None
}

/// Resolve the root of the reporting process's branch beneath a terminal
/// runtime. Unlike the short-lived reporter itself, this process represents
/// the command that owns the automatic integration.
pub(super) fn descendant_root(pid: u32, ancestor: u32) -> Option<u32> {
    let lineage = lineage(pid, ancestor)?;
    if lineage.len() <= 2 {
        Some(ancestor)
    } else {
        lineage.get(lineage.len() - 2).copied()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineage_reaches_the_requested_ancestor() {
        let pid = std::process::id();
        let lineage = lineage(pid, pid).unwrap();
        assert_eq!(lineage, [pid]);
        assert_eq!(descendant_root(pid, pid), Some(pid));
    }

    #[test]
    fn lineage_rejects_an_unrelated_process() {
        assert!(lineage(std::process::id(), u32::MAX).is_none());
    }
}
