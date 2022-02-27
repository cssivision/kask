use std::any::Any;
use std::cell::RefCell;
use std::io;
use std::mem;
use std::panic;
use std::rc::Rc;
use std::task::Waker;

use io_uring::squeue::Entry;
use io_uring::{cqueue, IoUring};
use scoped_tls::scoped_thread_local;
use slab::Slab;

pub mod accept;
pub mod action;
pub mod connect;
pub mod packet;
pub mod read;
pub mod recv;
pub mod recvmsg;
pub mod send;
pub mod sendmsg;
pub mod stream;
pub mod timeout;
pub mod write;

pub use action::Action;
pub use packet::Packet;
pub use read::Read;
pub use recv::Recv;
pub use recvmsg::RecvMsg;
pub use send::Send;
pub use sendmsg::SendMsg;
pub use stream::Stream;
pub use timeout::Timeout;
pub use write::Write;

pub const DEFAULT_BUFFER_SIZE: usize = 4096;

scoped_thread_local!(static CURRENT: Driver);

pub struct Driver {
    pub inner: Rc<RefCell<Inner>>,
}

impl Clone for Driver {
    fn clone(&self) -> Self {
        Driver {
            inner: self.inner.clone(),
        }
    }
}

pub struct Inner {
    ring: IoUring,
    actions: Slab<State>,
}

impl Driver {
    pub fn new() -> io::Result<Driver> {
        let ring = IoUring::new(256)?;
        // check if IORING_FEAT_FAST_POLL is supported
        if !ring.params().is_feature_fast_poll() {
            panic!("IORING_FEAT_FAST_POLL not supported");
        }

        let driver = Driver {
            inner: Rc::new(RefCell::new(Inner {
                ring,
                actions: Slab::with_capacity(256),
            })),
        };
        Ok(driver)
    }

    pub fn wait(&self) -> io::Result<()> {
        let inner = &mut *self.inner.borrow_mut();
        let ring = &mut inner.ring;

        if let Err(e) = ring.submit_and_wait(1) {
            if e.raw_os_error() == Some(libc::EBUSY) {
                return Ok(());
            }
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(e);
        }

        let mut cq = ring.completion();
        cq.sync();
        for cqe in cq {
            let key = cqe.user_data();
            if key == u64::MAX {
                continue;
            }
            let action = &mut inner.actions[key as usize];
            if action.complete(cqe) {
                inner.actions.remove(key as usize);
            }
        }

        Ok(())
    }

    pub fn with<T>(&self, f: impl FnOnce() -> T) -> T {
        CURRENT.set(self, f)
    }

    pub fn submit(&self, sqe: Entry) -> io::Result<usize> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        let key = inner.actions.insert(State::Submitted);

        let ring = &mut inner.ring;
        if ring.submission().is_full() {
            ring.submit()?;
            ring.submission().sync();
        }

        let sqe = sqe.user_data(key as u64);
        unsafe {
            ring.submission().push(&sqe).expect("push entry fail");
        }
        ring.submit()?;
        Ok(key)
    }
}

#[derive(Debug)]
pub enum State {
    /// The operation has been submitted to uring and is currently in-flight
    Submitted,
    /// The submitter is waiting for the completion of the operation
    Waiting(Waker),
    /// The operation has completed.
    Completed(cqueue::Entry),
    /// Ignored
    Ignored(Box<dyn Any>),
}

impl State {
    pub fn complete(&mut self, cqe: cqueue::Entry) -> bool {
        match mem::replace(self, State::Submitted) {
            State::Submitted => {
                *self = State::Completed(cqe);
                false
            }
            State::Waiting(waker) => {
                *self = State::Completed(cqe);
                waker.wake();
                false
            }
            State::Ignored(..) => true,
            State::Completed(..) => unreachable!("invalid operation state"),
        }
    }
}
