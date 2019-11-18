use crate::future::poll_fn;
use crate::io::{AsyncRead, AsyncWrite};
use crate::net::unix::split::{split, ReadHalf, WriteHalf};
use crate::net::unix::ucred::{self, UCred};
use crate::net::util::IoResource;

use bytes::{Buf, BufMut};
use std::convert::TryFrom;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A structure representing a connected Unix socket.
///
/// This socket can be connected directly with `UnixStream::connect` or accepted
/// from a listener with `UnixListener::incoming`. Additionally, a pair of
/// anonymous Unix sockets can be created with `UnixStream::pair`.
pub struct UnixStream {
    io: IoResource<mio::net::UnixStream>,
}

impl UnixStream {
    /// Connects to the socket named by `path`.
    ///
    /// This function will create a new Unix socket and connect to the path
    /// specified, associating the returned stream with the default event loop's
    /// handle.
    pub async fn connect<P>(path: P) -> io::Result<UnixStream>
    where
        P: AsRef<Path>,
    {
        let stream = mio::net::UnixStream::connect(path)?;
        let stream = UnixStream::new(stream)?;

        poll_fn(|cx| stream.io.poll_write_ready(cx)).await?;
        Ok(stream)
    }

    /// Consumes a `UnixStream` in the standard library and returns a
    /// nonblocking `UnixStream` from this crate.
    ///
    /// The returned stream will be associated with the given event loop
    /// specified by `handle` and is ready to perform I/O.
    pub fn from_std(stream: net::UnixStream) -> io::Result<UnixStream> {
        let stream = mio::net::UnixStream::from_std(stream);
        let io = IoResource::new(stream)?;

        Ok(UnixStream { io })
    }

    /// Creates an unnamed pair of connected sockets.
    ///
    /// This function will create a pair of interconnected Unix sockets for
    /// communicating back and forth between one another. Each socket will
    /// be associated with the default event loop's handle.
    pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
        let (a, b) = mio::net::UnixStream::pair()?;
        let a = UnixStream::new(a)?;
        let b = UnixStream::new(b)?;

        Ok((a, b))
    }

    pub(crate) fn new(stream: mio::net::UnixStream) -> io::Result<UnixStream> {
        let io = IoResource::new(stream)?;
        Ok(UnixStream { io })
    }

    /// Returns the socket address of the local half of this connection.
    pub fn local_addr(&self) -> io::Result<mio::unix::SocketAddr> {
        self.io.get_ref().local_addr()
    }

    /// Returns the socket address of the remote half of this connection.
    pub fn peer_addr(&self) -> io::Result<mio::unix::SocketAddr> {
        self.io.get_ref().peer_addr()
    }

    /// Returns effective credentials of the process which called `connect` or `pair`.
    pub fn peer_cred(&self) -> io::Result<UCred> {
        ucred::get_peer_cred(self)
    }

    /// Returns the value of the `SO_ERROR` option.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.io.get_ref().take_error()
    }

    /// Shuts down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O calls on the
    /// specified portions to immediately return with an appropriate value
    /// (see the documentation of `Shutdown`).
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.io.get_ref().shutdown(how)
    }

    /// Split a `UnixStream` into a read half and a write half, which can be used
    /// to read and write the stream concurrently.
    ///
    /// See the module level documenation of [`split`](super::split) for more
    /// details.
    pub fn split(&mut self) -> (ReadHalf<'_>, WriteHalf<'_>) {
        split(self)
    }
}

impl TryFrom<UnixStream> for mio::net::UnixStream {
    type Error = io::Error;

    /// Consumes value, returning the mio I/O object.
    ///
    /// See [`IoResource::into_inner`] for more details about
    /// resource deregistration that happens during the call.
    ///
    /// [`IoResource::into_inner`]: crate::util::PollEvented::into_inner
    fn try_from(value: UnixStream) -> Result<Self, Self::Error> {
        value.io.into_inner()
    }
}

impl TryFrom<net::UnixStream> for UnixStream {
    type Error = io::Error;

    /// Consumes stream, returning the tokio I/O object.
    ///
    /// This is equivalent to
    /// [`UnixStream::from_std(stream)`](UnixStream::from_std).
    fn try_from(stream: net::UnixStream) -> io::Result<Self> {
        Self::from_std(stream)
    }
}

impl AsyncRead for UnixStream {
    unsafe fn prepare_uninitialized_buffer(&self, _: &mut [u8]) -> bool {
        false
    }

    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_read_priv(cx, buf)
    }

    fn poll_read_buf<B: BufMut>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<usize>> {
        self.poll_read_buf_priv(cx, buf)
    }
}

impl AsyncWrite for UnixStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_priv(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_write_buf<B: Buf>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<usize>> {
        self.poll_write_buf_priv(cx, buf)
    }
}

impl UnixStream {
    // == Poll IO functions that takes `&self` ==
    //
    // They are not public because (taken from the doc of `PollEvented`):
    //
    // While `PollEvented` is `Sync` (if the underlying I/O type is `Sync`), the
    // caller must ensure that there are at most two tasks that use a
    // `PollEvented` instance concurrently. One for reading and one for writing.
    // While violating this requirement is "safe" from a Rust memory model point
    // of view, it will result in unexpected behavior in the form of lost
    // notifications and tasks hanging.

    pub(crate) fn poll_read_priv(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        ready!(self.io.poll_read_ready(cx))?;

        match self.io.get_ref().read(buf) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.io.clear_read_ready(cx)?;
                Poll::Pending
            }
            x => Poll::Ready(x),
        }
    }

    pub(crate) fn poll_read_buf_priv<B: BufMut>(
        &self,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<usize>> {
        ready!(self.io.poll_read_ready(cx))?;

        match unsafe { self.io.get_ref().read(buf.bytes_mut()) } {
            Ok(n) => {
                unsafe {
                    buf.advance_mut(n);
                }
                Poll::Ready(Ok(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.io.clear_read_ready(cx)?;
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    pub(crate) fn poll_write_priv(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        ready!(self.io.poll_write_ready(cx))?;

        match self.io.get_ref().write(buf) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.io.clear_write_ready(cx)?;
                Poll::Pending
            }
            x => Poll::Ready(x),
        }
    }

    pub(crate) fn poll_write_buf_priv<B: Buf>(
        &self,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<usize>> {
        ready!(self.io.poll_write_ready(cx))?;

        match self.io.get_ref().write(buf.bytes()) {
            Ok(n) => {
                buf.advance(n);
                Poll::Ready(Ok(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.io.clear_write_ready(cx)?;
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl fmt::Debug for UnixStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.io.get_ref().fmt(f)
    }
}

impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.io.get_ref().as_raw_fd()
    }
}
