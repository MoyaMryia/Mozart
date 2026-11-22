//! Abort a stalled SSE connection without waiting for its TCP send queue to drain.
use axum::{extract::connect_info::Connected, serve::IncomingStream};
use futures_util::task::AtomicWaker;
use std::{io, net::SocketAddr, os::fd::AsRawFd, pin::Pin, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, task::{Context, Poll}, time::Duration};
use tokio::{io::{AsyncRead, AsyncWrite, ReadBuf}, net::{TcpListener, TcpStream}};

#[derive(serde::Serialize)]
struct AcceptHealth { retrying: bool, last_error: Option<String> }
static ACCEPT_HEALTH: Mutex<AcceptHealth> = Mutex::new(AcceptHealth { retrying: false, last_error: None });
const ACCEPT_BACKOFF: Duration = Duration::from_millis(250);
pub fn health() -> serde_json::Value {
    serde_json::to_value(&*ACCEPT_HEALTH.lock().expect("accept health lock poisoned")).expect("accept health is serializable")
}
struct Control { aborted: AtomicBool, waker: AtomicWaker }
#[derive(Clone)]
pub struct Transport(Arc<Control>);
impl Transport {
    pub(crate) fn close(&self) { self.0.aborted.store(true, Ordering::Release); self.0.waker.wake(); }
}
pub struct Listener { inner: TcpListener }
impl Listener {
    pub fn from_std(listener: std::net::TcpListener) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        Ok(Self { inner: TcpListener::from_std(listener)? })
    }
    pub async fn bind(address: SocketAddr) -> io::Result<Self> { Ok(Self { inner: TcpListener::bind(address).await? }) }
    pub fn local_addr(&self) -> io::Result<SocketAddr> { self.inner.local_addr() }
}
pub struct Connection { inner: TcpStream, transport: Transport, abort_linger: bool }
impl Connection {
    fn set_abort_linger(&mut self) {
        if self.abort_linger || !self.transport.0.aborted.load(Ordering::Acquire) { return; }
        self.abort_linger = true;
        let linger = libc::linger { l_onoff: 1, l_linger: 0 };
        // The connection owns this live socket. Zero linger discards queued data on close.
        let result = unsafe { libc::setsockopt(self.inner.as_raw_fd(), libc::SOL_SOCKET, libc::SO_LINGER,
            (&linger as *const libc::linger).cast(), std::mem::size_of_val(&linger) as libc::socklen_t) };
        if result != 0 { eprintln!("SSE abort linger: {}", io::Error::last_os_error()); }
    }
    fn ready(&mut self, cx: &Context<'_>) -> io::Result<()> {
        self.transport.0.waker.register(cx.waker());
        if self.transport.0.aborted.load(Ordering::Acquire) {
            self.set_abort_linger();
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, "SSE client buffer exceeded"))
        } else { Ok(()) }
    }
}
impl Drop for Connection { fn drop(&mut self) { self.set_abort_linger(); } }
impl axum::serve::Listener for Listener {
    type Io = Connection;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((inner, address)) => {
                    ACCEPT_HEALTH.lock().expect("accept health lock poisoned").retrying = false;
                    return (Connection { inner, transport: Transport(Arc::new(Control { aborted: AtomicBool::new(false), waker: AtomicWaker::new() })), abort_linger: false }, address);
                },
                Err(error) if matches!(error.kind(), io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted) => eprintln!("TCP accept interrupted: {error}"),
                Err(error) if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)) => {
                    {
                        let mut health = ACCEPT_HEALTH.lock().expect("accept health lock poisoned");
                        health.retrying = true; health.last_error = Some(error.to_string());
                    }
                    eprintln!("TCP accept temporarily exhausted resources: {error}");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                },
                Err(error) => panic!("TCP accept failed: {error}"),
            }
        }
    }
    fn local_addr(&self) -> io::Result<Self::Addr> { self.inner.local_addr() }
}
impl Connected<IncomingStream<'_, Listener>> for Transport {
    fn connect_info(stream: IncomingStream<'_, Listener>) -> Self { stream.io().transport.clone() }
}
impl AsyncRead for Connection {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.ready(cx) { return Poll::Ready(Err(error)); }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl AsyncWrite for Connection {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if let Err(error) = self.ready(cx) { return Poll::Ready(Err(error)); }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.ready(cx) { return Poll::Ready(Err(error)); }
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.ready(cx) { return Poll::Ready(Err(error)); }
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
