// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use errno::{set_errno, Errno};
use s2n_tls::raw::{
    config::Config,
    connection::{Builder, Connection},
    enums::{CallbackResult, Mode},
    error::Error,
};
use std::{
    fmt,
    future::Future,
    io,
    os::raw::{c_int, c_void},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Clone)]
pub struct TlsAcceptor<B: Builder = Config>
where
    <B as Builder>::Output: Unpin,
{
    builder: B,
}

impl<B: Builder> TlsAcceptor<B>
where
    <B as Builder>::Output: Unpin,
{
    pub fn new(builder: B) -> Self {
        TlsAcceptor { builder }
    }

    pub async fn accept<S>(&self, stream: S) -> Result<TlsStream<S, B::Output>, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let conn = self.builder.build_connection(Mode::Server)?;
        TlsStream::open(conn, stream).await
    }
}

#[derive(Clone)]
pub struct TlsConnector<B: Builder = Config>
where
    <B as Builder>::Output: Unpin,
{
    builder: B,
}

impl<B: Builder> TlsConnector<B>
where
    <B as Builder>::Output: Unpin,
{
    pub fn new(builder: B) -> Self {
        TlsConnector { builder }
    }

    pub async fn connect<S>(
        &self,
        domain: &str,
        stream: S,
    ) -> Result<TlsStream<S, B::Output>, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut conn = self.builder.build_connection(Mode::Client)?;
        conn.as_mut().set_server_name(domain)?;
        TlsStream::open(conn, stream).await
    }
}

struct TlsHandshake<'a, S, C>
where
    C: AsRef<Connection> + AsMut<Connection> + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    tls: &'a mut TlsStream<S, C>,
}

impl<S, C> Future for TlsHandshake<'_, S, C>
where
    C: AsRef<Connection> + AsMut<Connection> + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        self.tls.with_io(ctx, |context| {
            let conn = context.get_mut().as_mut();
            conn.negotiate().map(|r| r.map(|_| ()))
        })
    }
}

pub struct TlsStream<S, C = Connection>
where
    C: AsRef<Connection> + AsMut<Connection> + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    conn: C,
    stream: S,
}

impl<S, C> TlsStream<S, C>
where
    C: AsRef<Connection> + AsMut<Connection> + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    async fn open(conn: C, stream: S) -> Result<Self, Error> {
        let mut tls = TlsStream { conn, stream };
        TlsHandshake { tls: &mut tls }.await?;
        Ok(tls)
    }

    fn with_io<F, R>(&mut self, ctx: &mut Context, action: F) -> Poll<Result<R, Error>>
    where
        F: FnOnce(Pin<&mut Self>) -> Poll<Result<R, Error>>,
    {
        // Setting contexts on a connection is considered unsafe
        // because the raw pointers provide no lifetime or memory guarantees.
        // We protect against this by pinning the stream during the action
        // and clearing the context afterwards.
        unsafe {
            let context = self as *mut Self as *mut c_void;

            self.as_mut().set_receive_callback(Some(Self::recv_io_cb))?;
            self.as_mut().set_send_callback(Some(Self::send_io_cb))?;
            self.as_mut().set_receive_context(context)?;
            self.as_mut().set_send_context(context)?;
            self.as_mut().set_waker(Some(ctx.waker()))?;

            let result = action(Pin::new(self));

            self.as_mut().set_receive_callback(None)?;
            self.as_mut().set_send_callback(None)?;
            self.as_mut().set_receive_context(std::ptr::null_mut())?;
            self.as_mut().set_send_context(std::ptr::null_mut())?;
            self.as_mut().set_waker(None)?;
            result
        }
    }

    fn poll_io<F>(ctx: *mut c_void, action: F) -> c_int
    where
        F: FnOnce(Pin<&mut S>, &mut Context) -> Poll<Result<usize, std::io::Error>>,
    {
        debug_assert_ne!(ctx, std::ptr::null_mut());
        let tls = unsafe { &mut *(ctx as *mut Self) };

        let mut async_context = Context::from_waker(tls.conn.as_ref().waker().unwrap());
        let stream = Pin::new(&mut tls.stream);

        match action(stream, &mut async_context) {
            Poll::Ready(Ok(len)) => len as c_int,
            Poll::Pending => {
                set_errno(Errno(libc::EWOULDBLOCK));
                CallbackResult::Failure.into()
            }
            _ => CallbackResult::Failure.into(),
        }
    }

    unsafe extern "C" fn recv_io_cb(ctx: *mut c_void, buf: *mut u8, len: u32) -> c_int {
        Self::poll_io(ctx, |stream, async_context| {
            let mut dest = ReadBuf::new(std::slice::from_raw_parts_mut(buf, len as usize));
            stream
                .poll_read(async_context, &mut dest)
                .map_ok(|_| dest.filled().len())
        })
    }

    unsafe extern "C" fn send_io_cb(ctx: *mut c_void, buf: *const u8, len: u32) -> c_int {
        Self::poll_io(ctx, |stream, async_context| {
            let src = std::slice::from_raw_parts(buf, len as usize);
            stream.poll_write(async_context, src)
        })
    }
}

impl<S, C> AsRef<Connection> for TlsStream<S, C>
where
    C: AsRef<Connection> + AsMut<Connection> + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn as_ref(&self) -> &Connection {
        self.conn.as_ref()
    }
}

impl<S, C> AsMut<Connection> for TlsStream<S, C>
where
    C: AsRef<Connection> + AsMut<Connection> + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn as_mut(&mut self) -> &mut Connection {
        self.conn.as_mut()
    }
}

impl<S, C> AsyncRead for TlsStream<S, C>
where
    C: AsRef<Connection> + AsMut<Connection> + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.get_mut()
            .with_io(ctx, |mut context| {
                context
                    .conn
                    .as_mut()
                    .recv(buf.initialize_unfilled())
                    .map_ok(|size| {
                        buf.advance(size);
                    })
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

impl<S, C> AsyncWrite for TlsStream<S, C>
where
    C: AsRef<Connection> + AsMut<Connection> + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut()
            .with_io(ctx, |mut context| context.conn.as_mut().send(buf))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    fn poll_flush(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let tls = self.get_mut();
        let tls_flush = tls
            .with_io(ctx, |mut context| {
                context.conn.as_mut().flush().map(|r| r.map(|_| ()))
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e));
        if tls_flush.is_ready() {
            Pin::new(&mut tls.stream).poll_flush(ctx)
        } else {
            tls_flush
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let tls = self.get_mut();
        let tls_shutdown = tls
            .with_io(ctx, |mut context| {
                context.conn.as_mut().shutdown().map(|r| r.map(|_| ()))
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e));
        if tls_shutdown.is_ready() {
            Pin::new(&mut tls.stream).poll_shutdown(ctx)
        } else {
            tls_shutdown
        }
    }
}

impl<S, C> fmt::Debug for TlsStream<S, C>
where
    C: AsRef<Connection> + AsMut<Connection> + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TlsStream")
            .field("connection", self.as_ref())
            .finish()
    }
}