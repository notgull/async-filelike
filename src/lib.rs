//! Asynchronous handle API.
//!
//! For socket-like system resources, the [`async-io`] crate provides a wrapper that
//! allows the resource to be used in asynchronous programs. However, this strategy
//! does not work with file-like objects, such as files, pipes and the standard output.
//! Usually, the strategy is to offload these objects onto a [thread pool]. However,
//! operating systems *do* provide APIs in certain cases for asynchronous file I/O;
//! they're just difficult to fit into the asynchronous object model.
//!
//! This crate provides wrappers around file-like objects that allows them to be used
//! in asynchronous programs; in many cases, without thread pools. These wrappers
//! include:
//!
//! - [`Handle`]: A lower level wrapper around file-like objects. It provides an API
//!   that allows one to pass a buffer into the object and then wait for the operation
//!   to complete. However, users may find this API inconvenient to use in cases where
//!   types are expected to implement [`AsyncRead`] or [`AsyncWrite`].
//! - [`Adaptor`]: A higher level wrapper around file-like objects. It provides an API
//!   similar to that of [`Async`] from the [`async-io`] crate. However, it uses an
//!   internal buffer to avoid the need for users to pass in a buffer. For smaller
//!   reads this is usually fine; however, for larger reads it may be more efficient
//!   to use [`Handle`].
//!
//! [thread pool]: https://crates.io/crates/blocking
//! [`async-io`]: https://crates.io/crates/async-io
//! [`AsyncRead`]: https://docs.rs/futures-io/latest/futures_io/trait.AsyncRead.html
//! [`AsyncWrite`]: https://docs.rs/futures-io/latest/futures_io/trait.AsyncWrite.html
//! [`Async`]: https://docs.rs/async-io/latest/async_io/struct.Async.html
//!
//! ## Strategy
//!
//! On Linux and Windows, completion APIs provided by the operating system are used.
//! For instance, [`IOCP`] is used on Windows, and [`io_uring`] is used on Linux. These
//! APIs are truly asynchronous and provide a way to wait for I/O operations to complete
//! without blocking a thread. For more information, see the [`submission`] crate.
//!
//! On other operating systems, file operations fall back to the [`blocking`] thread
//! pool. This ensures that the API is still usable on these platforms, but it does
//! mean that the operations are not truly asynchronous.
//!
//! [`IOCP`]: https://docs.microsoft.com/en-us/windows/win32/fileio/i-o-completion-ports
//! [`io_uring`]: https://kernel.dk/io_uring.pdf
//! [`submission`]: https://crates.io/crates/submission
//! [`blocking`]: https://crates.io/crates/blocking
//!
//! ## Performance
//!
//! In practical cases, reading through the [`Adaptor`] wrapper is significantly faster
//! than reading through the [`blocking`] thread pool. In certain cases, reading is a full
//! order of magnitude faster. However, writing is not as fast. This is because the
//! completion-based systems are optimized for reads but not writes. At best, writing is
//! as fast as the [`blocking`] thread pool. At worst, it is up to three times as slow.
//!
//! For read-heavy workloads, the [`Adaptor`] wrapper is a good choice. For write-heavy
//! workloads, the [`blocking`] thread pool may be better.

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};

use std::collections::HashMap;
use std::io::{self, prelude::*, SeekFrom};
use std::mem;
use std::ops;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::task::{Context, Poll};
use std::time::Duration;

use async_io::Timer;
use async_lock::Mutex;
use blocking::Unblock;
use futures_lite::{prelude::*, ready};
use once_cell::sync::Lazy;
use submission::{AsRaw, AsyncParameter, Completion, Operation, OperationBuilder, Raw, Ring};

#[doc(inline)]
pub use submission::{Buf, BufMut};

/// The runtime dictating how to handle the submission queue.
struct Runtime {
    /// The inner interface to io_uring/IOCP.
    ring: Ring,

    /// Mutex holding the buffer for events.
    ///
    /// Holding this lock implies the exclusive right to poll the ring.
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

    /// Shorthand for get().unwrap().
    fn get_unchecked() -> &'static Self {
        Self::get().unwrap()
    }

    /// Get an `OperationBuilder` for this runtime.
    fn operation(&self) -> OperationBuilder {
        unsafe { Operation::<'static, ()>::with_key(self.next_id.fetch_add(1, SeqCst)) }
    }

    /// Register a new source.
    fn register(
        &self,
        source: &impl AsRaw,
    ) -> io::Result<Raw> {
        log::trace!("Runtime: registering new source");
        self.ring.register(source)
    }

    /// Submit an `Operation` to the runtime.
    ///
    /// # Safety
    ///
    /// The operation must not be moved or forgotten.
    unsafe fn submit<T: AsyncParameter>(
        &'static self,
        operation: Pin<&mut Operation<'static, T>>,
    ) -> io::Result<()> {
        self.ring.submit(operation)
    }

    /// Wait for an operation to occur.
    async fn pump_events(&self, key: u64) -> io::Result<Completion> {
        // Backoff for timeouts.
        const BACKOFF: &[u64] = &[
            2, 3, 5, 10, 50, 75, 100, 250, 500, 750, 1000, 2500, 5000, 10_000,
        ];

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
                log::trace!("pump_events: timeout while polling for events");
                io::Result::Ok(true)
            };

            // Poll the ring for events.
            let poller = async { self.ring.wait(&mut events.buffer).await.map(|len| len == 0) };

            log::trace!("pump_events: waiting for events");
            let snoozed = poller.or(timeout).await?;

            if snoozed {
                backoff_index = backoff_index.saturating_add(1);
            } else {
                // Drain the buffer into the map. If we see our event, return it.
                let mut our_completion = None;
                let Events { buffer, events } = &mut *events;

                log::trace!("pump_events: found {} events", buffer.len());

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

                // Reset the backoff.
                backoff_index = 0;
            }

            // Try again. Drop the lock as well so other tasks can look for their events.
        }
    }
}

/// Async adaptor for file handle types.
///
/// This type will do one of two things:
///
/// - If a completion-based runtime is available, it will register the object into it
///   and use it to run asynchronous operations.
/// - Otherwise, it will use a thread pool to run blocking operations.
///
/// ## Caveats
///
/// `Handle` is a lower-level primitive. It does not support reading to and writing from
/// arbitrary buffers; only specific, static buffers can be used. See the [`Buf`] and
/// [`BufMut`] traits for more information.
///
/// For a primitive with higher-level functionality, see the [`Adaptor`] type.
///
/// ## Supported Types
///
/// On Unix-type operating systems, this crate should theoretically support any system
/// resource type. Even socket types can be used in this wrapper. However, it may be more
/// efficient to use the [`Async`] type for sockets.
///
/// On Windows, only file handles are supported. These file handles usually implement
/// [`AsRawHandle`]. However, theoretically, sockets should also be able to be used in
/// this type.
///
/// [`Async`]: https://docs.rs/async-io/latest/async_io/struct.Async.html
/// [`AsRawHandle`]: https://doc.rust-lang.org/std/os/windows/io/trait.AsRawHandle.html
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
    pub fn new(io: T) -> io::Result<Self> {
        if let Some(rt) = Runtime::get() {
            let handle = rt.register(&io)?;

            Ok(Self(Repr::Submission {
                raw: RawContainer(handle),
                io: Some(io),
            }))
        } else {
            Ok(Self(Repr::Blocking(Unblock::new(io))))
        }
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> Handle<T> {
    /// Create a new `Handle`.
    pub fn new(io: T) -> io::Result<Self> {
        if let Some(rt) = Runtime::get() {
            let handle = rt.register(&io)?;

            Ok(Self(Repr::Submission {
                raw: RawContainer(handle),
                io: Some(io),
            }))
        } else {
            Ok(Self(Repr::Blocking(Unblock::new(io))))
        }
    }
}

impl<T: Send + 'static> Handle<T> {
    /// Run the operation on either the thread pool or the submission queue.
    async fn run_operation<R: Send + 'static, B: AsyncParameter + Send + 'static>(
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
                // Create an operation.
                let io = io.as_mut().unwrap();
                let operation =
                    get_operation(Runtime::get_unchecked().operation(), io, raw, buffer);
                let key = operation.key();

                // Pin the operation to the heap. Even if this is forgotten, it doesn't matter, since the user
                // loses access.
                let mut operation = Box::pin(operation);

                // Submit the operation.
                unsafe {
                    Runtime::get_unchecked().submit(operation.as_mut()).ok();
                }

                // Wait for the operation to complete.
                let completion = Runtime::get_unchecked().pump_events(key).await;

                let mut completion = match completion {
                    Ok(completion) => completion,
                    Err(err) => {
                        // Cancel the operation and return.
                        let op_unlocked = operation
                            .as_mut()
                            .cancel()
                            .expect("Failed to cancel operation");
                        return (op_unlocked.buffer_mut().0.take().unwrap(), Err(err));
                    }
                };

                // Unlock the operation and move out the buffer.
                let op_unlocked = operation.as_mut().unlock(&completion).unwrap();
                let buffer = op_unlocked.buffer_mut().0.take().unwrap();
                let result = completion.result();

                (buffer, result.map(cvt_result))
            }
        }
    }
}

impl<T> Handle<T> {
    /// Get a mutable reference to the raw handle.
    ///
    /// This is an asynchronous method because the underlying handle may be in use by another
    /// thread, and this method will block until it is available.
    pub async fn get_mut(&mut self) -> &mut T {
        match &mut self.0 {
            Repr::Blocking(io) => io.get_mut().await,
            Repr::Submission { io, .. } => io.as_mut().unwrap(),
        }
    }

    /// Extracts the inner handle.
    ///
    /// This is an asynchronous method because the underlying handle may be in use by another
    /// thread, and this method will block until it is available.
    pub async fn into_inner(mut self) -> T {
        match self.0 {
            Repr::Blocking(io) => io.into_inner().await,
            Repr::Submission { ref mut io, .. } => io.take().unwrap(),
        }
    }
}

impl<T: Read + MaybeSeek + Send + 'static> Handle<T> {
    /// Read into the given buffer at the given offset.
    pub async fn read_into<B: AsMut<[u8]> + BufMut + Send + 'static>(
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

impl<T: Write + MaybeSeek + Send + 'static> Handle<T> {
    /// Write from the buffer at the given offset.
    pub async fn write_from<B: AsRef<[u8]> + Buf + Send + 'static>(
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
///
/// This provides a higher-level interface to [`Handle`] that is compatible with
/// the asynchronous I/O traits in the [`futures`] crate.
///
/// ## Trait Implementations
///
/// Since `Adaptor` uses an internal buffer, it implements `BufRead` for `T: Read`. Use
/// the [`Adaptor::with_capacity`] or [`Adaptor::set_capacity`] methods to control the
/// size of the buffer.
///
/// [`futures`]: https://crates.io/crates/futures
pub struct Adaptor<T>(State<T>);

enum State<T> {
    /// We are not doing anything.
    Idle(Idle<T>),

    /// We are reading.
    Reading(Task<T>),

    /// We are writing.
    Writing(Task<T>),

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
    Pin<Box<dyn Future<Output = (Handle<T>, Vec<u8>, u64, io::Result<usize>)> + Send + 'static>>;

impl<T> From<Handle<T>> for Adaptor<T> {
    fn from(val: Handle<T>) -> Self {
        Self::with_handle_and_capacity(0, val)
    }
}

#[cfg(unix)]
impl<T: AsRawFd> Adaptor<T> {
    /// Create a new `Adaptor`.
    pub fn new(io: T) -> io::Result<Self> {
        Handle::new(io).map(Into::into)
    }

    /// Create a new `Adaptor` with the given capacity.
    pub fn with_capacity(capacity: usize, io: T) -> io::Result<Self> {
        Ok(Self::with_handle_and_capacity(capacity, Handle::new(io)?))
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> Adaptor<T> {
    /// Create a new `Adaptor`.
    pub fn new(io: T) -> io::Result<Self> {
        Handle::new(io).map(Into::into)
    }

    /// Create a new `Adaptor` with the given capacity.
    pub fn with_capacity(capacity: usize, io: T) -> io::Result<Self> {
        Ok(Self::with_handle_and_capacity(capacity, Handle::new(io)?))
    }
}

impl<T> Adaptor<T> {
    /// Create a new `Adaptor` from a [`Handle`], with the given buffer capacity.
    pub fn with_handle_and_capacity(capacity: usize, io: Handle<T>) -> Self {
        Self(State::Idle(Idle {
            io,
            buffer: Vec::with_capacity(capacity),
            offset: 0,
        }))
    }
}

impl<T: Send + Read + MaybeSeek + Unpin + 'static> AsyncRead for Adaptor<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let len = <[u8]>::len(buf);

        loop {
            match &mut self.0 {
                State::Hole => unreachable!("Cannot poll an empty hole"),
                State::Idle(idle) => match idle.io.0 {
                    Repr::Blocking(ref mut bl) => {
                        // We're using blocking I/O. Read directly.
                        return Pin::new(bl).poll_read(cx, buf);
                    }
                    _ => {
                        // Preform an asynchronous read.
                        let idle = match mem::replace(&mut self.0, State::Hole) {
                            State::Idle(idle) => idle,
                            _ => unreachable!(),
                        };

                        // Begin the read.
                        let Idle {
                            mut io,
                            mut buffer,
                            offset,
                        } = idle;

                        // Resize the buffer if needed.
                        if buffer.len() < len {
                            buffer.resize(len, 0);
                        }

                        let task = async move {
                            let (buffer, res) = io.read_into(buffer, len, offset).await;
                            (io, buffer, offset, res)
                        };

                        // Begin polling it.
                        self.0 = State::Reading(Box::pin(task));
                    }
                },
                State::Reading(task) => {
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
                        }
                        Err(_) => 0,
                    };

                    self.0 = State::Idle(Idle {
                        io,
                        buffer,
                        offset: offset.saturating_add(increase as u64),
                    });

                    // Return the result.
                    return Poll::Ready(res);
                }
                State::Writing(_) => panic!("Attempted to read while writing"),
            }
        }
    }
}

impl<T: Send + Write + MaybeSeek + Unpin + 'static> AsyncWrite for Adaptor<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match &mut self.0 {
                State::Hole => unreachable!("Cannot poll an empty hole"),
                State::Idle(idle) => match idle.io.0 {
                    Repr::Blocking(ref mut bl) => {
                        // We're using blocking I/O. Write directly.
                        return Pin::new(bl).poll_write(cx, buf);
                    }
                    _ => {
                        // Preform an asynchronous write.
                        let idle = match mem::replace(&mut self.0, State::Hole) {
                            State::Idle(idle) => idle,
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
                        self.0 = State::Writing(Box::pin(task));
                    }
                },
                State::Writing(task) => {
                    // Poll the task for completion.
                    let (io, buffer, offset, res) = ready!(task.poll(cx));

                    // Move ourself back into place.
                    let increase = *res.as_ref().unwrap_or(&0);

                    self.0 = State::Idle(Idle {
                        io,
                        buffer,
                        offset: offset.saturating_add(increase as u64),
                    });

                    // Return the result.
                    return Poll::Ready(res);
                }
                State::Reading(_) => panic!("Attempted to write while reading"),
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.0 {
            State::Idle(idle) => {
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
        match &mut self.0 {
            State::Idle(idle) => {
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

impl<T: Send + Seek + Unpin + 'static> AsyncSeek for Adaptor<T> {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        match &mut self.0 {
            State::Idle(idle) => {
                match &mut idle.io.0 {
                    Repr::Blocking(ubl) => Pin::new(ubl).poll_seek(cx, pos),
                    Repr::Submission { .. } => {
                        // We can just adjust our cursor.
                        idle.offset = match pos {
                            SeekFrom::Start(offset) => offset,
                            SeekFrom::End(_) => {
                                return Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
                            }
                            SeekFrom::Current(offset) => {
                                (idle.offset as i64).saturating_add(offset) as u64
                            }
                        };

                        Poll::Ready(Ok(idle.offset))
                    }
                }
            }
            _ => panic!("Attempted to seek while reading or writing"),
        }
    }
}

/// An object that may or may not be able to be seeked into.
///
/// Certain objects, like files, are seekable. Others, like pipes, are not.
/// This trait is used to abstract over the two. You can use the [`WontSeek`]
/// type to wrap around an object that can't seek.
pub trait MaybeSeek {
    /// Seek to the specified location, or do nothing.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<Option<u64>>;
}

impl<T: Seek + ?Sized> MaybeSeek for T {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<Option<u64>> {
        self.seek(pos).map(Some)
    }
}

/// An object whose seeking properties should be ignored.
///
/// Many functions in this crate are parameterized by an offset into the presumed file.
/// However, certain types, like standard input/output, are streams where offsets make
/// no sense. In this case, the type should be wrapped in `WontSeek` to indicate that
/// seeking should be ignored.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct WontSeek<T>(pub T);

impl<T> ops::Deref for WontSeek<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> ops::DerefMut for WontSeek<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> From<T> for WontSeek<T> {
    fn from(val: T) -> Self {
        Self(val)
    }
}

#[cfg(unix)]
impl<T: AsRawFd> AsRawFd for WontSeek<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> AsRawHandle for WontSeek<T> {
    fn as_raw_handle(&self) -> RawHandle {
        self.0.as_raw_handle()
    }
}

impl<T: Read> Read for WontSeek<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<T: Write> Write for WontSeek<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<T> MaybeSeek for WontSeek<T> {
    fn seek(&mut self, _pos: SeekFrom) -> io::Result<Option<u64>> {
        Ok(None)
    }
}

/// A buffer that can be moved out.
struct MovableBuf<B>(Option<B>);

unsafe impl<B: AsyncParameter> AsyncParameter for MovableBuf<B> {
    fn ptr(&self) -> Option<std::ptr::NonNull<u8>> {
        self.0.as_ref().and_then(|b| b.ptr())
    }

    fn ptr2(&self) -> Option<std::ptr::NonNull<u8>> {
        self.0.as_ref().and_then(|b| b.ptr2())
    }

    fn len(&self) -> usize {
        self.0.as_ref().map(|b| b.len()).unwrap_or(0)
    }

    fn len2(&self) -> usize {
        self.0.as_ref().map(|b| b.len2()).unwrap_or(0)
    }
}
unsafe impl<B: Buf> Buf for MovableBuf<B> {}
unsafe impl<B: BufMut> BufMut for MovableBuf<B> {}

impl<B> From<B> for MovableBuf<B> {
    fn from(buf: B) -> Self {
        Self(Some(buf))
    }
}

struct RawContainer(Raw);

unsafe impl Send for RawContainer {}
unsafe impl Sync for RawContainer {}

#[cfg(unix)]
impl AsRawFd for RawContainer {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawHandle for RawContainer {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.0
    }
}

#[cfg(not(any(unix, windows)))]
impl AsRaw for RawContainer {
    fn as_raw(&self) -> Raw {
        self.0
    }
}
