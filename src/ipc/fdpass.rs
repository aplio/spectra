//! SCM_RIGHTS file-descriptor passing over Unix stream sockets.
//!
//! Used by the live server handoff to move PTY master fds from the running
//! server to its successor. Each message carries a small payload plus up to
//! [`MAX_FDS_PER_MESSAGE`] descriptors as ancillary data; larger transfers
//! are sent as multiple messages.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// Maximum descriptors attached to one `sendmsg` call. Kept well under
/// `SCM_MAX_FD` (253 on Linux) so a batch never hits kernel limits.
pub const MAX_FDS_PER_MESSAGE: usize = 32;

/// Control-message buffer size: `CMSG_SPACE(32 * sizeof(int))` is ~144
/// bytes on Linux; 512 leaves generous headroom on every Unix.
const CMSG_BUFFER_LEN: usize = 512;

/// Aligned backing store for the ancillary (cmsg) data.
#[repr(C, align(8))]
struct CmsgBuffer([u8; CMSG_BUFFER_LEN]);

/// `recvmsg` flag that atomically opens received fds close-on-exec. Linux and
/// the BSDs provide `MSG_CMSG_CLOEXEC`; Apple platforms lack it, so there we
/// pass no flag and set `FD_CLOEXEC` by hand after the descriptors arrive.
#[cfg(not(target_vendor = "apple"))]
const RECV_CLOEXEC_FLAG: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(target_vendor = "apple")]
const RECV_CLOEXEC_FLAG: libc::c_int = 0;

/// Duplicate a raw fd into an owned close-on-exec descriptor. The clone is
/// what gets sent over the handoff socket, so the original stays untouched
/// no matter how the transfer ends.
pub fn dup_fd_cloexec(fd: RawFd) -> io::Result<OwnedFd> {
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl(F_DUPFD_CLOEXEC) returned a fresh fd we exclusively own.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

/// Send `payload` with `fds` attached as SCM_RIGHTS ancillary data.
///
/// The payload must be non-empty (ancillary data needs at least one data
/// byte) and small enough to go out in a single `sendmsg`.
pub fn send_with_fds(stream: &UnixStream, payload: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd-passing payload must not be empty",
        ));
    }
    if fds.is_empty() || fds.len() > MAX_FDS_PER_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "fd batch size {} out of range 1..={MAX_FDS_PER_MESSAGE}",
                fds.len()
            ),
        ));
    }

    let fd_bytes = mem::size_of_val(fds);
    let mut cmsg_buffer = CmsgBuffer([0u8; CMSG_BUFFER_LEN]);
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: payload.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buffer.0.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(fd_bytes as u32) } as _;

    // SAFETY: msg_control points at a zeroed, aligned buffer at least
    // CMSG_SPACE(fd_bytes) long, so the cmsg macros stay in bounds.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(header), fd_bytes);
    }

    loop {
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
        if sent < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if sent as usize != payload.len() {
            return Err(io::Error::other(format!(
                "short sendmsg: sent {sent} of {} payload bytes",
                payload.len()
            )));
        }
        return Ok(());
    }
}

/// Receive one message with any attached SCM_RIGHTS descriptors.
///
/// Returns the number of payload bytes read into `buf` (0 = EOF) and the
/// received fds, opened close-on-exec. Truncated ancillary data is an
/// error; any fds that did arrive are closed before returning it.
pub fn recv_with_fds(stream: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    if buf.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd-receiving buffer must not be empty",
        ));
    }

    let mut cmsg_buffer = CmsgBuffer([0u8; CMSG_BUFFER_LEN]);
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buffer.0.as_mut_ptr().cast();
    msg.msg_controllen = CMSG_BUFFER_LEN as _;

    let received = loop {
        let received =
            unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, RECV_CLOEXEC_FLAG) };
        if received < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        break received as usize;
    };

    let mut fds = Vec::new();
    // SAFETY: recvmsg filled msg_control/msg_controllen; the cmsg macros
    // walk only within that region.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data_len = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = data_len / mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(header);
                for index in 0..count {
                    let mut fd: RawFd = -1;
                    std::ptr::copy_nonoverlapping(
                        data.add(index * mem::size_of::<RawFd>()),
                        (&mut fd as *mut RawFd).cast::<u8>(),
                        mem::size_of::<RawFd>(),
                    );
                    if fd >= 0 {
                        // Apple platforms have no MSG_CMSG_CLOEXEC, so the fd
                        // arrived without close-on-exec. Set it now, before the
                        // OwnedFd owns it, so nothing leaks across a fork+exec.
                        #[cfg(target_vendor = "apple")]
                        {
                            let flags = libc::fcntl(fd, libc::F_GETFD);
                            if flags >= 0 {
                                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                            }
                        }
                        fds.push(OwnedFd::from_raw_fd(fd));
                    }
                }
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }

    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        // Dropping `fds` closes whatever partially arrived.
        return Err(io::Error::other(
            "ancillary data truncated while receiving fds",
        ));
    }

    Ok((received, fds))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    use super::{MAX_FDS_PER_MESSAGE, dup_fd_cloexec, recv_with_fds, send_with_fds};

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0i32; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "pipe() failed");
        unsafe {
            use std::os::fd::FromRawFd;
            (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
        }
    }

    #[test]
    fn passes_three_usable_fds_with_payload_over_socketpair() {
        let (sender, receiver) = UnixStream::pair().expect("socketpair");

        let pipes: Vec<(OwnedFd, OwnedFd)> = (0..3).map(|_| pipe_pair()).collect();
        let raw: Vec<i32> = pipes.iter().map(|(read, _)| read.as_raw_fd()).collect();
        send_with_fds(&sender, b"F", &raw).expect("send fds");

        let mut buf = [0u8; 8];
        let (n, received) = recv_with_fds(&receiver, &mut buf).expect("recv fds");
        assert_eq!(&buf[..n], b"F");
        assert_eq!(received.len(), 3);

        // Each received fd must be usable: write through the original pipe
        // write end and read the bytes back through the transferred fd.
        for (index, fd) in received.iter().enumerate() {
            let marker = format!("fd-{index}");
            let mut write_end = std::fs::File::from(pipes[index].1.try_clone().expect("clone"));
            write_end.write_all(marker.as_bytes()).expect("write pipe");
            drop(write_end);

            let mut read_end = std::fs::File::from(fd.try_clone().expect("clone received fd"));
            let mut got = vec![0u8; marker.len()];
            read_end.read_exact(&mut got).expect("read via received fd");
            assert_eq!(got, marker.as_bytes());
        }
    }

    #[test]
    fn rejects_empty_and_oversized_batches() {
        let (sender, _receiver) = UnixStream::pair().expect("socketpair");
        let err = send_with_fds(&sender, b"F", &[]).expect_err("empty batch rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

        let (read, _write) = pipe_pair();
        let oversized = vec![read.as_raw_fd(); MAX_FDS_PER_MESSAGE + 1];
        let err = send_with_fds(&sender, b"F", &oversized).expect_err("oversized batch rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

        let err = send_with_fds(&sender, b"", &[read.as_raw_fd()]).expect_err("empty payload");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn recv_reports_eof_as_zero_bytes_without_fds() {
        let (sender, receiver) = UnixStream::pair().expect("socketpair");
        drop(sender);
        let mut buf = [0u8; 4];
        let (n, fds) = recv_with_fds(&receiver, &mut buf).expect("recv at eof");
        assert_eq!(n, 0);
        assert!(fds.is_empty());
    }

    #[test]
    fn dup_fd_cloexec_produces_independent_descriptor() {
        let (read, write) = pipe_pair();
        let dup = dup_fd_cloexec(read.as_raw_fd()).expect("dup");
        assert_ne!(dup.as_raw_fd(), read.as_raw_fd());
        drop(read);

        let mut write_end = std::fs::File::from(write);
        write_end.write_all(b"still-open").expect("write");
        drop(write_end);

        let mut read_end = std::fs::File::from(dup);
        let mut got = String::new();
        read_end.read_to_string(&mut got).expect("read via dup");
        assert_eq!(got, "still-open");
    }
}
