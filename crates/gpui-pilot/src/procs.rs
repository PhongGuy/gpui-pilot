//! Finds a sandboxed app's pid as the server's pid namespace sees it.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

/// One process as `/proc/<pid>/status` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// The pid in each nested namespace, the reader's own first, the innermost last.
    pub nspid: Vec<u32>,
}

/// Parses `PPid:` and `NSpid:` from a status text; `None` when either is missing or malformed.
pub(crate) fn parse_status(pid: u32, status: &str) -> Option<Proc> {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
    };
    let ppid = field("PPid")?.trim().parse().ok()?;
    let nspid = field("NSpid")?
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<Vec<u32>, _>>()
        .ok()?;
    if nspid.is_empty() {
        return None;
    }
    Some(Proc { pid, ppid, nspid })
}

/// Every numeric entry under `proc_root` whose `status` reads and parses; others are skipped.
pub(crate) fn scan(proc_root: &Path) -> Vec<Proc> {
    // Processes come and go while we read, so any entry that fails to read or parse is
    // simply left out rather than failing the scan.
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let status = std::fs::read_to_string(entry.path().join("status")).ok()?;
            parse_status(pid, &status)
        })
        .collect()
}

/// The one descendant of `root` (root excluded) whose NSpid has at least two entries and ends
/// in `inner`, by its first NSpid entry; `None` for zero or several.
pub(crate) fn find_host_pid(root: u32, inner: u32, procs: &[Proc]) -> Option<u32> {
    // Walk the tree breadth-first from the root's children, collecting every process that
    // lives in a nested namespace and carries the app's inner pid there. A scan torn by pid
    // reuse can hold a parent cycle, so each pid is visited once.
    let mut seen = HashSet::from([root]);
    let mut queue = VecDeque::from([root]);
    let mut found = Vec::new();
    while let Some(parent) = queue.pop_front() {
        for child in procs.iter().filter(|p| p.ppid == parent) {
            if !seen.insert(child.pid) {
                continue;
            }
            if child.nspid.len() >= 2 && child.nspid.last() == Some(&inner) {
                found.push(child.nspid[0]);
            }
            queue.push_back(child.pid);
        }
    }
    match found[..] {
        [one] => Some(one),
        _ => None,
    }
}

/// `find_host_pid` over `scan("/proc")`.
pub(crate) fn host_pid(root: u32, inner: u32) -> Option<u32> {
    find_host_pid(root, inner, &scan(Path::new("/proc")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, nspid: &[u32]) -> Proc {
        Proc {
            pid,
            ppid,
            nspid: nspid.to_vec(),
        }
    }

    #[test]
    fn a_status_text_gives_its_parent_and_namespace_pids() {
        let status = "Name:\tpilot-demo-wind\nTgid:\t4242\nPid:\t4242\nPPid:\t4200\n\
                      NSpid:\t4242\t3\t7\nThreads:\t12\n";
        assert_eq!(
            parse_status(4242, status),
            Some(proc(4242, 4200, &[4242, 3, 7]))
        );
        let flat = "Name:\tsh\nPPid:\t1\nNSpid:\t99\n";
        assert_eq!(parse_status(99, flat), Some(proc(99, 1, &[99])));
        assert_eq!(
            parse_status(5, "Name:\tx\nPPid:\t1\n"),
            None,
            "no NSpid line"
        );
        assert_eq!(parse_status(5, "PPid:\tone\nNSpid:\t5\n"), None, "bad PPid");
        assert_eq!(
            parse_status(5, "PPid:\t1\nNSpid:\t5\tx\n"),
            None,
            "bad NSpid"
        );
        assert_eq!(parse_status(5, ""), None, "a half-written status");
    }

    #[test]
    fn the_app_is_the_descendant_whose_innermost_pid_matches() {
        // bwrap 100 under the server, its init 101 (inner 1), agent 102 (inner 2), app 103
        // (inner 7). A second sandbox under 200 also has an inner 7, at 203.
        let procs = [
            proc(100, 50, &[100]),
            proc(101, 100, &[101, 1]),
            proc(102, 101, &[102, 2]),
            proc(103, 102, &[103, 7]),
            proc(200, 50, &[200]),
            proc(201, 200, &[201, 1]),
            proc(203, 201, &[203, 7]),
        ];
        assert_eq!(find_host_pid(100, 7, &procs), Some(103));
        assert_eq!(find_host_pid(200, 7, &procs), Some(203));
    }

    #[test]
    fn a_deeper_nesting_still_ends_in_the_inner_pid() {
        // pasta 300 holds bwrap 301 in its namespace (inner 1); the app is 7 inside bwrap's.
        let procs = [
            proc(300, 50, &[300]),
            proc(301, 300, &[301, 1]),
            proc(302, 301, &[302, 2, 1]),
            proc(305, 302, &[305, 5, 7]),
        ];
        assert_eq!(find_host_pid(300, 7, &procs), Some(305));
    }

    #[test]
    fn several_candidates_or_none_give_no_pid() {
        // A single-level NSpid equal to the inner pid is the server's own namespace, not a
        // sandbox, so it never matches.
        let flat = [proc(100, 50, &[100]), proc(7, 100, &[7])];
        assert_eq!(find_host_pid(100, 7, &flat), None);
        // Nothing under the root ends in 7.
        let none = [proc(100, 50, &[100]), proc(101, 100, &[101, 1])];
        assert_eq!(find_host_pid(100, 7, &none), None);
        // Two depths of one sandbox both end in 7: ambiguous.
        let several = [
            proc(100, 50, &[100]),
            proc(101, 100, &[101, 7]),
            proc(102, 101, &[102, 3, 7]),
        ];
        assert_eq!(find_host_pid(100, 7, &several), None);
        // A matching process that is not under the root is ignored.
        let outside = [proc(100, 50, &[100]), proc(400, 50, &[400, 7])];
        assert_eq!(find_host_pid(100, 7, &outside), None);
    }

    #[test]
    fn scan_reads_numeric_entries_and_skips_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let write = |name: &str, status: Option<&str>| {
            let dir = root.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            if let Some(status) = status {
                std::fs::write(dir.join("status"), status).unwrap();
            }
        };
        write("12", Some("PPid:\t1\nNSpid:\t12\t3\n"));
        write("13", None);
        write("14", Some("PPid:\t1\n"));
        write("self", Some("PPid:\t1\nNSpid:\t99\n"));
        assert_eq!(scan(root.path()), [proc(12, 1, &[12, 3])]);
    }

    #[test]
    fn the_live_proc_lists_this_test_process() {
        let me = std::process::id();
        let found = scan(Path::new("/proc"));
        let mine = found
            .iter()
            .find(|p| p.pid == me)
            .expect("this process is listed");
        assert_eq!(mine.nspid.last(), Some(&me));
    }
}
