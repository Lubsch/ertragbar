use rustix::net;

/// Read and discard everything queued on a nonblocking socket.
pub(crate) fn drain(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    let mut buf = [0u8; 4096];
    let mut got = false;
    while let Ok((n, _)) = net::recv(fd, &mut buf, net::RecvFlags::DONTWAIT) {
        if n > 0 {
            got = true;
        }
    }
    got
}
