//! Best-effort foreground process resolution via `/proc` (Linux only).
//!
//! Never panics; any parse or IO failure yields `None`.

/// Parse the foreground process group id (`tpgid`, field 8) from a
/// `/proc/<pid>/stat` line. The comm field (field 2) may contain spaces and
/// parentheses, so parsing starts after the last `)`.
fn tpgid_from_stat(stat: &str) -> Option<i32> {
    let (_, after_comm) = stat.rsplit_once(')')?;
    // Fields after comm: state ppid pgrp session tty_nr tpgid ...
    after_comm.split_whitespace().nth(5)?.parse().ok()
}

/// Extract argv[0]'s basename from a NUL-separated `/proc/<pid>/cmdline`.
fn argv0_basename(cmdline: &[u8]) -> Option<String> {
    let argv0_bytes = cmdline.split(|byte| *byte == 0).next()?;
    if argv0_bytes.is_empty() {
        return None;
    }
    let argv0 = String::from_utf8_lossy(argv0_bytes);
    let basename = argv0.rsplit('/').next().unwrap_or(&argv0);
    if basename.is_empty() {
        None
    } else {
        Some(basename.to_string())
    }
}

/// Resolve the foreground process name of the terminal owned by `child_pid`:
/// `/proc/<child>/stat` tpgid -> `/proc/<tpgid>/cmdline` -> argv[0] basename.
#[cfg(target_os = "linux")]
pub fn foreground_process_name(child_pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat")).ok()?;
    let tpgid = tpgid_from_stat(&stat)?;
    let tpgid = u32::try_from(tpgid).ok()?;
    if tpgid == 0 {
        return None;
    }
    let cmdline = std::fs::read(format!("/proc/{tpgid}/cmdline")).ok()?;
    argv0_basename(&cmdline)
}

#[cfg(not(target_os = "linux"))]
pub fn foreground_process_name(_child_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tpgid_is_parsed_from_fabricated_stat_line() {
        // pid (comm) state ppid pgrp session tty_nr tpgid ...
        let stat = "1234 (bash) S 1000 1234 1234 34816 4321 4194304 1000";
        assert_eq!(tpgid_from_stat(stat), Some(4321));
    }

    #[test]
    fn tpgid_parsing_survives_comm_with_spaces_and_parens() {
        let stat = "99 (tmux: client) (x) S 1 99 99 34816 777 0";
        assert_eq!(tpgid_from_stat(stat), Some(777));
    }

    #[test]
    fn tpgid_parsing_rejects_truncated_or_garbage_lines() {
        assert_eq!(tpgid_from_stat(""), None);
        assert_eq!(tpgid_from_stat("1234 (bash) S 1 2"), None);
        assert_eq!(tpgid_from_stat("no parens at all"), None);
        assert_eq!(tpgid_from_stat("1 (a) S x y z w not-a-number"), None);
    }

    #[test]
    fn negative_tpgid_means_no_foreground_group() {
        let stat = "1234 (daemon) S 1 1234 1234 0 -1 4194304";
        assert_eq!(tpgid_from_stat(stat), Some(-1));
        // foreground_process_name filters this via the u32 conversion.
        assert!(u32::try_from(-1i32).is_err());
    }

    #[test]
    fn argv0_basename_takes_first_nul_component() {
        assert_eq!(
            argv0_basename(b"/usr/local/bin/claude\0--continue\0"),
            Some("claude".to_string())
        );
        assert_eq!(argv0_basename(b"claude\0"), Some("claude".to_string()));
        assert_eq!(argv0_basename(b""), None);
        assert_eq!(argv0_basename(b"/usr/bin/\0"), None);
    }
}
