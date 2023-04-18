use std::future::Future;
use std::io;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use io_uring::{opcode, types};

use crate::driver::Action;

#[allow(dead_code)]
pub(crate) struct Write {
    buf: Vec<u8>,
}

impl Action<Write> {
    pub(crate) fn write(fd: RawFd, buf: &[u8]) -> io::Result<Action<Write>> {
        let buf = buf.to_vec();
        let write = Write { buf };
        let entry =
            opcode::Write::new(types::Fd(fd), write.buf.as_ptr(), write.buf.len() as u32).build();
        Action::submit(write, entry)
    }

    pub(crate) fn poll_write(&mut self, cx: &mut Context) -> Poll<io::Result<usize>> {
        let complete = ready!(Pin::new(self).poll(cx));
        let n = complete.result? as usize;
        Poll::Ready(Ok(n))
    }
}
