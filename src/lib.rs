//! Asynchronous handle API.

#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use std::collections::HashMap;
use std::io::{self, prelude::*, SeekFrom};
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::task::{Context, Poll};
use std::time::Duration;

use async_io::Timer;
use async_lock::Mutex;
use blocking::Unblock;
use futures_lite::{pin, prelude::*, ready};
use once_cell::sync::Lazy;
use submission::{AsRaw, Buf, Completion, Operation, OperationBuilder, Raw, Ring};

/// The runtime dictating how to handle the submission queue.
struct Runtime {
    /// The inner interface to io_uring/IOCP.
    ring: Ring,

    /// Mutex holding the buffer for events.
    ///
    /// Holding this lock implies the exclusive right to poll it.
    events: Mutex<Events>,

    /// The next operation ID to use.
    next_id: AtomicU64,
}

/// Event buffer for the `Runtime`.
#[derive(Default)]
struct Events {
    /// Raw buffer for completion events.
    buffer: Vec<Completion>,

    /// Events organized by their key.
    events: HashMap<u64, Completion>,
}

impl Runtime {
    /// Get the global `Runtime`.
    ///
    /// If one is not available, returns `None`.
    fn get() -> Option<&'static Self> {
        static GLOBAL_RUNTIME: Lazy<Option<Runtime>> = Lazy::new(|| {
            // Try to create a ring.
            match Ring::new() {
                Ok(ring) => {
                    // We can create a ring, so create a runtime.
                    Some(Runtime {
                        ring,
                        events: Mutex::new(Events::default()),
                        next_id: AtomicU64::new(0),
                    })
                }
                Err(err) => {
                    // We can't create a ring, so we can't create a runtime.
                    log::info!("Unable to use completion-based API: {}", err);
                    None
                }
            }
        });

        GLOBAL_RUNTIME.as_ref()
    }

    /// Is the runtime available?
    fn available() -> bool {
        Self::get().is_some()
    }

    /// Shorthand for get().unwrap().
    fn get_unchecked() -> &'static Self {
        Self::get().unwrap()
    }

    /// Get an `OperationBuilder` for this runtime.
    fn operation(&self) -> OperationBuilder {
        unsafe { Operation::<'static, ()>::with_key(self.next_id.fetch_add(1, SeqCst)) }
    }

    /// Submit an `Operation` to the runtime.
    fn submit<T: Buf>(&'static self, operation: Pin<&mut Operation<'static, T>>) -> io::Result<()> {
        self.ring.submit(operation)
    }

    /// Wait for an operation to occur.
    async fn pump_events(&self, key: u64) -> io::Result<Completion> {
        // Exponential backoff for timeouts.
        const BACKOFF: &[u64] = &[50, 75, 100, 250, 500, 750, 1000, 2500, 5000, 10_000];

        let mut backoff_index = 0;

        loop {
            // Lock the event list.
            let mut events = self.events.lock().await;

            // If the event is in the list, return it.
            if let Some(event) = events.events.remove(&key) {
                return Ok(event);
            }

            // Timeout for polling, to make sure other tasks have a chance to check for events.
            // This helps event polling fairness immediately after a large batch of events are delivered.
            let timer = BACKOFF
                .get(backoff_index)
                .map_or_else(Timer::never, |t| Timer::after(Duration::from_micros(*t)));
            let timeout = async {
                timer.await;
                Ok(false)
            };

            // Poll the ring for events.
            let poller = async { self.ring.wait(&mut events.buffer).await.map(|len| len > 0) };

            let snoozed = poller.or(timeout).await?;

            if snoozed {
                backoff_index = backoff_index.saturating_add(1);
            } else {
                // Drain the buffer into the map. If we see our event, return it.
                let mut our_completion = None;
                let Events { buffer, events } = &mut *events;

                for completion in buffer.drain(..) {
                    if completion.key() == key {
                        our_completion = Some(completion);
                    } else {
                        events.insert(completion.key(), completion);
                    }
                }

                // If we found our completion, return it.
                if let Some(completion) = our_completion {
                    return Ok(completion);
                }
            }

            // Try again. Drop the lock as well so other tasks can look for their events.
        }
    }
}

/// Async adaptor for file handle types.
pub struct Handle<T>(Repr<T>);

enum Repr<T> {
    /// The runtime is not available and we are backed by a thread pool.
    Blocking(Unblock<T>),

    /// The runtime is available and we are backed by it.
    Submission {
        /// The actual I/O object.
        io: Option<T>,

        /// The raw handle of this object.
        raw: RawContainer,
    },
}

#[cfg(unix)]
impl<T: AsRawFd> Handle<T> {
    /// Create a new `Handle`.
    pub fn new(io: T) -> Self {
        if Runtime::available() {
            Self(Repr::Submission {
                raw: RawContainer(io.as_raw()),
                io: Some(io),
            })
        } else {
            Self(Repr::Blocking(Unblock::new(io)))
        }
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> Handle<T> {
    /// Create a new `Handle`.
    pub fn new(io: T) -> Self {
        if Runtime::available() {
            Self(Repr::Submission {
                raw: RawContainer(io.as_raw()),
                io: Some(io),
            })
        } else {
            Self(Repr::Blocking(Unblock::new(io)))
        }
    }
}

impl<T: Send + 'static> Handle<T> {
    /// Run the operation on either the thread pool or the submission queue.
    async fn run_operation<R: Send + 'static, B: AsRef<[u8]> + Send + 'static>(
        &mut self,
        buffer: B,
        threadpool_op: impl FnOnce(&mut T, B) -> (B, io::Result<R>) + Send + 'static,
        get_operation: impl FnOnce(
            OperationBuilder,
            &mut T,
            &RawContainer,
            B,
        ) -> Operation<'static, MovableBuf<B>>,
        cvt_result: impl FnOnce(isize) -> R,
    ) -> (B, io::Result<R>) {
        match &mut self.0 {
            Repr::Blocking(io) => {
                // Just block on it.
                io.with_mut(move |io| threadpool_op(io, buffer)).await
            }
            Repr::Submission { io, raw } => {
                // Create an operation and pin it to the stack.
                let io = io.as_mut().unwrap();
                let operation =
                    get_operation(Runtime::get_unchecked().operation(), io, raw, buffer);
                let key = operation.key();
                pin!(operation);

                // Submit the operation.
                Runtime::get_unchecked().submit(operation.as_mut()).ok();

                // Wait for the operation to complete.
                let completion = Runtime::get_unchecked().pump_events(key).await;

                let mut completion = match completion {
                    Ok(completion) => completion,
                    Err(err) => {
                        // Cancel the operation and return.
                        let op_unlocked = operation.cancel().expect("Failed to cancel operation");
                        return (op_unlocked.buffer_mut().0.take().unwrap(), Err(err));
                    }
                };

                // Unlock the operation and move out the buffer.
                let op_unlocked = operation.unlock(&completion).unwrap();
                let buffer = op_unlocked.buffer_mut().0.take().unwrap();
                let result = completion.result();

                (buffer, result.map(cvt_result))
            }
        }
    }
}

impl<T: Read + Seek + Send + 'static> Handle<T> {
    /// Read into the given buffer at the given offset.
    pub async fn read_into<B: AsRef<[u8]> + AsMut<[u8]> + Send + 'static>(
        &mut self,
        buf: B,
        len: usize,
        offset: u64,
    ) -> (B, io::Result<usize>) {
        self.run_operation(
            buf,
            move |io, mut buf| {
                let result = {
                    if let Err(e) = io.seek(SeekFrom::Start(offset)) {
                        return (buf, Err(e));
                    }

                    let buf_slice = match buf.as_mut().get_mut(..len) {
                        Some(buf_slice) => buf_slice,
                        None => buf.as_mut(),
                    };
                    io.read(buf_slice)
                };

                (buf, result)
            },
            |op, _, raw, buf| op.read(raw, MovableBuf::from(buf), offset),
            |result| result as usize,
        )
        .await
    }
}

impl<T: Write + Seek + Send + 'static> Handle<T> {
    /// Write from the buffer at the given offset.
    pub async fn write_from<B: AsRef<[u8]> + Send + 'static>(
        &mut self,
        buf: B,
        len: usize,
        offset: u64,
    ) -> (B, io::Result<usize>) {
        self.run_operation(
            buf,
            move |io, buf| {
                let result = {
                    if let Err(e) = io.seek(SeekFrom::Start(offset)) {
                        return (buf, Err(e));
                    }

                    let buf_slice = match buf.as_ref().get(..len) {
                        Some(buf_slice) => buf_slice,
                        None => buf.as_ref(),
                    };
                    io.write(buf_slice)
                };

                (buf, result)
            },
            |op, _, raw, buf| op.write(raw, MovableBuf::from(buf), offset),
            |result| result as usize,
        )
        .await
    }
}

/// An adaptor around a [`Handle`] that implements asynchronous I/O traits.
pub struct Adaptor<T> {
    state: State<T>,
}

enum State<T> {
    /// We are not doing anything.
    Idle { idle: Idle<T> },

    /// We are reading.
    Reading { task: Task<T> },

    /// We are writing.
    Writing { task: Task<T> },

    /// Temporary placeholder value.
    Hole,
}

/// An `Adaptor` at rest.
struct Idle<T> {
    /// The inner I/O handle.
    io: Handle<T>,

    /// A flexible buffer for reads and writes.
    buffer: Vec<u8>,

    /// The current offset.
    offset: u64,
}

type Task<T> =
    Pin<Box<dyn Future<Output = (Handle<T>, Vec<u8>, u64, io::Result<usize>)> + 'static>>;

impl<T> From<Handle<T>> for Adaptor<T> {
    fn from(val: Handle<T>) -> Self {
        Self {
            state: State::Idle {
                idle: Idle {
                    io: val,
                    buffer: Vec::new(),
                    offset: 0,
                },
            },
        }
    }
}

#[cfg(unix)]
impl<T: AsRawFd> Adaptor<T> {
    /// Create a new `Adaptor`.
    pub fn new(io: T) -> Self {
        Handle::new(io).into()
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> Adaptor<T> {
    /// Create a new `Adaptor`.
    pub fn new(io: T) -> Self {
        Handle::new(io).into()
    }
}

impl<T: Send + Read + Seek + Unpin + 'static> AsyncRead for Adaptor<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.state {
                State::Hole => unreachable!("Cannot poll an empty hole"),
                State::Idle { idle } => match idle.io.0 {
                    Repr::Blocking(ref mut bl) => {
                        // We're using blocking I/O. Read directly.
                        return Pin::new(bl).poll_read(cx, buf);
                    }
                    _ => {
                        // Preform an asynchronous read.
                        let idle = match mem::replace(&mut self.state, State::Hole) {
                            State::Idle { idle } => idle,
                            _ => unreachable!(),
                        };

                        // Begin the read.
                        let Idle {
                            mut io,
                            mut buffer,
                            offset,
                        } = idle;

                        // Resize the buffer if needed.
                        if buffer.len() < buf.len() {
                            buffer.resize(buf.len(), 0);
                        }
                        let len = buf.len();

                        let task = async move {
                            let (buffer, res) = io.read_into(buffer, len, offset).await;
                            (io, buffer, offset, res)
                        };

                        // Begin polling it.
                        self.state = State::Reading {
                            task: Box::pin(task),
                        };
                    }
                },
                State::Reading { task } => {
                    // Poll the task for completion.
                    let (io, buffer, offset, res) = ready!(task.poll(cx));

                    // Move ourself back into place.
                    let increase = match res {
                        Ok(increase) => {
                            // Copy from the buffer to the output buffer.
                            let buf_slice = match buf.get_mut(..increase) {
                                Some(buf_slice) => buf_slice,
                                None => buf,
                            };
                            buf_slice.copy_from_slice(&buffer[..increase]);
                            increase
                        },
                        Err(_) => 0,
                    };

                    self.state = State::Idle {
                        idle: Idle {
                            io,
                            buffer,
                            offset: offset.saturating_add(increase as u64),
                        },
                    };

                    // Return the result.
                    return Poll::Ready(res);
                }
                State::Writing { .. } => panic!("Attempted to read while writing"),
            }
        }
    }
}

impl<T: Send + Write + Seek + Unpin + 'static> AsyncWrite for Adaptor<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.state {
                State::Hole => unreachable!("Cannot poll an empty hole"),
                State::Idle { idle } => match idle.io.0 {
                    Repr::Blocking(ref mut bl) => {
                        // We're using blocking I/O. Write directly.
                        return Pin::new(bl).poll_write(cx, buf);
                    }
                    _ => {
                        // Preform an asynchronous write.
                        let idle = match mem::replace(&mut self.state, State::Hole) {
                            State::Idle { idle } => idle,
                            _ => unreachable!(),
                        };

                        // Begin the write.
                        let Idle {
                            mut io,
                            mut buffer,
                            offset,
                        } = idle;

                        // Copy `buf` into our own buffer.
                        buffer.clear();
                        buffer.extend_from_slice(buf); 
                        let len = buf.len();

                        let task = async move {
                            let (buffer, res) = io.write_from(buffer, len, offset).await;
                            (io, buffer, offset, res)
                        };

                        // Begin polling it.
                        self.state = State::Writing {
                            task: Box::pin(task),
                        };
                    }
                },
                State::Writing { task } => {
                    // Poll the task for completion.
                    let (io, buffer, offset, res) = ready!(task.poll(cx));

                    // Move ourself back into place.
                    let increase = *res.as_ref().unwrap_or(&0);

                    self.state = State::Idle {
                        idle: Idle {
                            io,
                            buffer,
                            offset: offset.saturating_add(increase as u64),
                        },
                    };

                    // Return the result.
                    return Poll::Ready(res);
                }
                State::Reading { .. } => panic!("Attempted to write while reading"),
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.state {
            State::Idle { idle } => {
                match &mut idle.io.0 {
                    Repr::Blocking(ubl) => Pin::new(ubl).poll_flush(cx),
                    Repr::Submission { io, .. } => {
                        // Preform a flush.
                        io.as_mut().unwrap().flush().map(|_| ()).into()
                    }
                }
            }
            _ => panic!("Attempted to flush while reading or writing"),
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.state {
            State::Idle { idle } => {
                match &mut idle.io.0 {
                    Repr::Blocking(ubl) => Pin::new(ubl).poll_close(cx),
                    Repr::Submission { .. } => {
                        // Nothing to do.
                        Poll::Ready(Ok(()))
                    }
                }
            }
            _ => panic!("Attempted to flush while reading or writing"),
        }
    }
}

struct MovableBuf<B>(Option<B>);

impl<B: AsRef<[u8]>> AsRef<[u8]> for MovableBuf<B> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref().unwrap().as_ref()
    }
}

impl<B: AsMut<[u8]>> AsMut<[u8]> for MovableBuf<B> {
    fn as_mut(&mut self) -> &mut [u8] {
        self.0.as_mut().unwrap().as_mut()
    }
}

impl<B> From<B> for MovableBuf<B> {
    fn from(buf: B) -> Self {
        Self(Some(buf))
    }
}

struct RawContainer(Raw);

#[cfg(unix)]
impl AsRawFd for RawContainer {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawHandle for RawContainer {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.0.as_raw_handle()
    }
}

#[cfg(not(any(unix, windows)))]
impl AsRaw for RawContainer {
    fn as_raw(&self) -> Raw {
        self.0
    }
}
