use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io;
use std::mem;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use io_uring::squeue::Entry;
use io_uring::{cqueue, opcode, types, IoUring};
use scoped_tls::scoped_thread_local;
use slab::Slab;

use crate::buffer::{Buf, BufRing, Builder};

mod op;

pub(crate) use op::*;

pub const BUF_BGID: u16 = 666;
const DEFAULT_RING_ENTRIES: u16 = 128;
const DEFAULT_BUF_CNT: u16 = 128;
const DEFAULT_BUF_LEN: usize = 4096;
const DEFAULT_TIME_OP_SIZE: usize = 1000;
const TIMER_KEY: u64 = u64::MAX - 1;
const RESERVE_MIN_KEY: u64 = u64::MAX - 1;

scoped_thread_local!(pub(crate) static CURRENT: Driver);

pub(crate) struct Driver {
    inner: Rc<RefCell<Inner>>,
}

impl Clone for Driver {
    fn clone(&self) -> Self {
        Driver {
            inner: self.inner.clone(),
        }
    }
}

struct Inner {
    bufgroup: BufRing,
    ring: IoUring,
    ops: Slab<State>,
    timer_ops: VecDeque<TimerOp>,
    timers: BTreeMap<(Instant, usize), Waker>,
    id: usize,
}

/// A single timer operation.
enum TimerOp {
    Insert(Instant, usize, Waker),
    Remove(Instant, usize),
}

impl Inner {
    fn new() -> io::Result<Inner> {
        let ring = IoUring::new(256)?;
        let bufgroup = Builder::new(BUF_BGID)
            .ring_entries(DEFAULT_RING_ENTRIES)
            .buf_cnt(DEFAULT_BUF_CNT)
            .buf_len(DEFAULT_BUF_LEN)
            .build()?;
        let mut inner = Inner {
            ring,
            ops: Slab::with_capacity(256),
            bufgroup,
            timer_ops: VecDeque::new(),
            timers: BTreeMap::new(),
            id: 0,
        };
        inner.register_buf_ring()?;
        Ok(inner)
    }

    fn register_buf_ring(&mut self) -> io::Result<()> {
        // Safety: The ring, represented by the ring_start and the ring_entries remains valid until
        // it is unregistered. The backing store is an AnonymousMmap which remains valid until it
        // is dropped which in this case, is when Self is dropped.
        let res = unsafe {
            self.ring.submitter().register_buf_ring(
                self.bufgroup.as_ptr() as _,
                self.bufgroup.ring_entries(),
                self.bufgroup.bgid(),
            )
        };

        if let Err(e) = res {
            match e.raw_os_error() {
                Some(libc::EINVAL) => {
                    // using buf_ring requires kernel 5.19 or greater.
                    return Err(io::Error::new(
                            io::ErrorKind::Other, format!(
                                "buf_ring.register returned {}, most likely indicating this kernel is not 5.19+", e),
                            ));
                }
                Some(libc::EEXIST) => {
                    // Registering a duplicate bgid is not allowed. There is an `unregister`
                    // operations that can remove the first, but care must be taken that there
                    // are no outstanding operations that will still return a buffer from that
                    // one.
                    return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!(
                                "buf_ring.register returned `{}`, indicating the attempted buffer group id {} was already registered",
                            e,
                            self.bufgroup.bgid()),
                        ));
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!(
                            "buf_ring.register returned `{}` for group id {}",
                            e,
                            self.bufgroup.bgid()
                        ),
                    ));
                }
            }
        };
        res
    }

    fn submit(&mut self, sqe: Entry) -> io::Result<()> {
        if self.ring.submission().is_full() {
            self.ring.submit()?;
            self.ring.submission().sync();
        }
        unsafe {
            self.ring.submission().push(&sqe).expect("push entry fail");
        }
        self.ring.submit()?;
        self.ring.submission().sync();
        Ok(())
    }

    fn wait(&mut self) -> io::Result<()> {
        let mut wakers = Vec::new();
        let timeout = self.process_timers(&mut wakers);

        // submit timeout op.
        if let Some(timeout) = timeout {
            let timespec = types::Timespec::new()
                .sec(timeout.as_secs())
                .nsec(timeout.subsec_nanos());
            let sqe = opcode::Timeout::new(&timespec as *const _)
                .build()
                .user_data(TIMER_KEY);
            self.submit(sqe)?;
        }
        let want = if timeout == Some(Duration::from_secs(0)) {
            0
        } else {
            1
        };
        if let Err(e) = self.ring.submit_and_wait(want) {
            if e.raw_os_error() == Some(libc::EBUSY) {
                return Ok(());
            }
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(e);
        }

        let mut cq = self.ring.completion();
        cq.sync();
        for cqe in cq {
            if cqe.user_data() >= RESERVE_MIN_KEY {
                continue;
            }
            let index = cqe.user_data() as _;
            let op = &mut self.ops[index];
            if op.complete(cqe) {
                self.ops.remove(index);
            }
        }

        if timeout != Some(Duration::from_secs(0)) {
            let _ = self.process_timers(&mut wakers);
        }
        // Wake up ready tasks.
        for waker in wakers {
            waker.wake();
        }
        Ok(())
    }

    fn submit_op<T>(&mut self, driver: Driver, op: T, sqe: Entry) -> io::Result<Op<T>> {
        let key = self.ops.insert(State::Submitted);
        let sqe = sqe.user_data(key as u64);
        self.submit(sqe)?;
        Ok(Op {
            driver,
            op: Some(op),
            key,
        })
    }

    fn get_buf(&self, result: u32, flags: u32) -> io::Result<Buf> {
        let bid = cqueue::buffer_select(flags).unwrap();
        let len = result as usize;
        let buf = self.bufgroup.get_buf(len, bid)?;
        Ok(buf)
    }

    fn insert_timer(&mut self, when: Instant, waker: &Waker) -> usize {
        let id = self.id;
        self.id = self.id.wrapping_add(1);
        self.timer_ops
            .push_back(TimerOp::Insert(when, id, waker.clone()));
        if self.timer_ops.len() >= DEFAULT_TIME_OP_SIZE {
            self.process_timer_ops();
        }
        id
    }

    fn remove_timer(&mut self, when: Instant, id: usize) {
        self.timer_ops.push_back(TimerOp::Remove(when, id));
        if self.timer_ops.len() >= DEFAULT_TIME_OP_SIZE {
            self.process_timer_ops();
        }
    }

    fn process_timers(&mut self, wakers: &mut Vec<Waker>) -> Option<Duration> {
        self.process_timer_ops();

        // Split timers into ready and pending timers.
        let now = Instant::now();
        let pending = self.timers.split_off(&(now, 0));
        let ready = mem::replace(&mut self.timers, pending);
        let dur = if ready.is_empty() {
            self.timers
                .keys()
                .next()
                .map(|(when, _)| when.saturating_duration_since(now))
        } else {
            Some(Duration::from_secs(0))
        };
        for (_, waker) in ready {
            wakers.push(waker);
        }
        dur
    }

    fn process_timer_ops(&mut self) {
        for _ in 0..self.timer_ops.capacity() {
            match self.timer_ops.pop_front() {
                Some(TimerOp::Insert(when, id, waker)) => {
                    self.timers.insert((when, id), waker);
                }
                Some(TimerOp::Remove(when, id)) => {
                    self.timers.remove(&(when, id));
                }
                None => break,
            }
        }
    }
}

impl Driver {
    pub(crate) fn new() -> io::Result<Driver> {
        Ok(Driver {
            inner: Rc::new(RefCell::new(Inner::new()?)),
        })
    }

    pub(crate) fn insert_timer(&self, when: Instant, waker: &Waker) -> usize {
        self.inner.borrow_mut().insert_timer(when, waker)
    }

    pub(crate) fn remove_timer(&self, when: Instant, id: usize) {
        self.inner.borrow_mut().remove_timer(when, id)
    }

    pub(crate) fn wait(&self) -> io::Result<()> {
        self.inner.borrow_mut().wait()
    }

    pub(crate) fn get_buf(&self, result: u32, flags: u32) -> io::Result<Buf> {
        self.inner.borrow().get_buf(result, flags)
    }

    pub(crate) fn with<T>(&self, f: impl FnOnce() -> T) -> T {
        CURRENT.set(self, f)
    }

    pub(crate) fn submit<T>(&self, op: T, sqe: Entry) -> io::Result<Op<T>> {
        self.inner.borrow_mut().submit_op(self.clone(), op, sqe)
    }
}

enum State {
    /// The operation has been submitted to uring and is currently in-flight
    Submitted,
    /// The submitter is waiting for the completion of the operation
    Waiting(Waker),
    /// The operation has completed.
    Completed(CqeResult),
    /// The operations list.
    CompletionList(Vec<CqeResult>),
    /// Ignored
    Ignored(Box<dyn Any>),
}

impl State {
    fn complete(&mut self, cqe: cqueue::Entry) -> bool {
        match mem::replace(self, State::Submitted) {
            s @ State::Submitted | s @ State::Waiting(..) => {
                if io_uring::cqueue::more(cqe.flags()) {
                    *self = State::CompletionList(vec![cqe.into()]);
                } else {
                    *self = State::Completed(cqe.into());
                }
                if let State::Waiting(waker) = s {
                    waker.wake();
                }
                false
            }
            s @ State::Ignored(..) => {
                if io_uring::cqueue::more(cqe.flags()) {
                    *self = s;
                    false
                } else {
                    true
                }
            }
            State::CompletionList(mut list) => {
                list.push(cqe.into());
                *self = State::CompletionList(list);
                false
            }
            State::Completed(..) => unreachable!("invalid state"),
        }
    }
}

pub(crate) trait Completable {
    type Output;
    /// `complete` will be called for cqe's do not have the `more` flag set
    fn complete(self, cqe: CqeResult) -> Self::Output;
    /// Update will be called for cqe's which have the `more` flag set.
    /// The Op should update any internal state as required.
    fn update(&mut self, _cqe: CqeResult) {}
}

pub(crate) struct Op<T: 'static> {
    pub driver: Driver,
    pub op: Option<T>,
    pub key: usize,
}

impl<T> Op<T> {
    pub(crate) fn get_mut(&mut self) -> &mut T {
        self.op.as_mut().unwrap()
    }

    pub(crate) fn get_buf(&self, cqe: CqeResult) -> io::Result<Buf> {
        let result = cqe.result?;
        let flags = cqe.flags;
        self.driver.get_buf(result, flags)
    }

    pub(crate) fn submit(op: T, entry: Entry) -> io::Result<Op<T>> {
        CURRENT.with(|driver| driver.submit(op, entry))
    }

    fn poll2(&mut self, cx: &mut Context) -> Poll<T::Output>
    where
        T: Completable,
    {
        let mut inner = self.driver.inner.borrow_mut();
        let state = inner.ops.get_mut(self.key).expect("invalid state key");

        match mem::replace(state, State::Submitted) {
            State::Submitted => {
                *state = State::Waiting(cx.waker().clone());
                Poll::Pending
            }
            State::Waiting(waker) => {
                if !waker.will_wake(cx.waker()) {
                    *state = State::Waiting(cx.waker().clone());
                } else {
                    *state = State::Waiting(waker);
                }
                Poll::Pending
            }
            State::Completed(cqe) => {
                inner.ops.remove(self.key);
                Poll::Ready(self.op.take().unwrap().complete(cqe))
            }
            State::CompletionList(list) => {
                let data = self.op.as_mut().unwrap();
                let mut status = None;
                let mut updated = false;
                for cqe in list.into_iter() {
                    if cqueue::more(cqe.flags) {
                        updated = true;
                        data.update(cqe);
                    } else {
                        status = Some(cqe);
                        break;
                    }
                }
                if updated {
                    // because we update internal state, wake and rerun the task.
                    cx.waker().wake_by_ref();
                }
                match status {
                    None => {
                        *state = State::Waiting(cx.waker().clone());
                    }
                    Some(cqe) => {
                        *state = State::Completed(cqe);
                    }
                }
                Poll::Pending
            }
            State::Ignored(..) => unreachable!(),
        }
    }
}

impl<T> Drop for Op<T> {
    fn drop(&mut self) {
        let mut inner = self.driver.inner.borrow_mut();
        let state = match inner.ops.get_mut(self.key) {
            Some(s) => s,
            None => return,
        };

        let mut finished = true;
        match state {
            State::Submitted | State::Waiting(_) => {
                finished = false;
                *state = State::Ignored(Box::new(self.op.take()));
            }
            State::Completed(..) => {
                inner.ops.remove(self.key);
            }
            State::CompletionList(list) => {
                let more = if !list.is_empty() {
                    cqueue::more(list.last().unwrap().flags)
                } else {
                    false
                };
                if more {
                    finished = false;
                    *state = State::Ignored(Box::new(self.op.take()));
                } else {
                    inner.ops.remove(self.key);
                }
            }
            State::Ignored(..) => unreachable!(),
        }
        if !finished {
            let sqe = opcode::AsyncCancel::new(self.key as u64)
                .build()
                .user_data(u64::MAX);
            let _ = inner.submit(sqe);
        }
    }
}

impl<T> Future for Op<T>
where
    T: Unpin + Completable,
{
    type Output = T::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.poll2(cx)
    }
}

#[allow(dead_code)]
pub(crate) struct CqeResult {
    pub(crate) result: io::Result<u32>,
    pub(crate) flags: u32,
}

impl From<cqueue::Entry> for CqeResult {
    fn from(cqe: cqueue::Entry) -> Self {
        let res = cqe.result();
        let flags = cqe.flags();
        let result = if res >= 0 {
            Ok(res as u32)
        } else {
            Err(io::Error::from_raw_os_error(-res))
        };
        CqeResult { result, flags }
    }
}
