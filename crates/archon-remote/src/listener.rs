//! The TCP accept loop: plain HTTP/1.1 on loopback, at most
//! `max_connections` connections served at once, heads read within
//! [`HEAD_TIMEOUT`]. A connection beyond the cap is answered `503` with
//! `Retry-After`, so the app can say it is retrying, and then closed. TLS is
//! the job of `tailscale serve` or a proxy.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinHandle;

use crate::Shared;

/// How long a head may take, and so how long an idle keep-alive connection
/// holds its slot: hyper restarts this timer for each next request's head.
/// A browser opens up to six connections per host to load the app's files;
/// they close this long after the burst, leaving each tab its WebSocket.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(3);
/// Connections answered `503` at once while every slot is taken; beyond
/// that a newcomer is dropped unanswered.
const MAX_REFUSALS: usize = 64;
/// How long a refused connection gets to send the head it is answered for.
pub const REFUSAL_HEAD_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a refused connection lives in all, writing the answer included,
/// so a client that never reads cannot hold a refusal permit.
pub const REFUSAL_LIFETIME: Duration = Duration::from_secs(4);
const MAX_HEAD_BYTES: usize = 32 * 1024;
/// How long open HTTP connections get to finish once the listener closes.
const DRAIN: Duration = Duration::from_secs(2);

/// An open listener; [`Running::stop`] closes it and its connections.
#[derive(Debug)]
pub struct Running {
    pub address: SocketAddr,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Running {
    pub async fn stop(self) {
        self.stop.send_replace(true);
        if let Err(error) = self.task.await {
            tracing::error!(%error, "the remote accept loop panicked");
        }
    }
}

/// Bind the configured address and start accepting.
pub fn start(shared: Arc<Shared>) -> Result<Running, ArchonError> {
    let wanted = shared.config.socket_address()?;
    let bound = std::net::TcpListener::bind(wanted)
        .and_then(|l| l.set_nonblocking(true).map(|()| l))
        .map_err(|e| ArchonError::io(format!("binding the remote listener on {wanted}"), e))?;
    let listener = TcpListener::from_std(bound)
        .map_err(|e| ArchonError::io("registering the remote listener", e))?;
    let address = listener
        .local_addr()
        .map_err(|e| ArchonError::io("reading the remote listener's address", e))?;
    assert!(
        address.ip().is_loopback() || shared.config.allow_remote,
        "a non-loopback bind needs allow_remote"
    );
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(accept_loop(shared, listener, address, stopped));
    Ok(Running {
        address,
        stop,
        task,
    })
}

async fn accept_loop(
    shared: Arc<Shared>,
    listener: TcpListener,
    bound: SocketAddr,
    mut stopped: watch::Receiver<bool>,
) {
    let slots = Arc::new(Semaphore::new(shared.config.max_connections as usize));
    let refusals = Arc::new(Semaphore::new(MAX_REFUSALS));
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = stopped.changed() => break,
            Some(_) = connections.join_next() => continue,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!(%error, "a remote connection was not accepted");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            refuse(&refusals, &mut connections, stream);
            continue;
        };
        let (shared, stopped) = (shared.clone(), stopped.clone());
        connections.spawn(async move {
            serve(shared, stream, bound, stopped).await;
            drop(slot);
        });
    }
    drop(listener);
    if tokio::time::timeout(DRAIN, drain(&mut connections))
        .await
        .is_err()
    {
        connections.shutdown().await;
    }
}

/// Every slot is taken: answer the newcomer's one request `503`, unless
/// [`MAX_REFUSALS`] are already being answered, then drop it.
fn refuse(
    refusals: &Arc<Semaphore>,
    connections: &mut tokio::task::JoinSet<()>,
    stream: TcpStream,
) {
    let Ok(permit) = refusals.clone().try_acquire_owned() else {
        tracing::debug!("every remote connection slot is taken; dropping a connection");
        return;
    };
    tracing::debug!("every remote connection slot is taken; answering 503");
    connections.spawn(answer_refusal(stream, permit));
}

/// Answer one refused connection's request `503` and close it within
/// [`REFUSAL_LIFETIME`], however slowly it reads; then give `permit` back.
async fn answer_refusal<I>(io: I, permit: OwnedSemaphorePermit)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(|request: hyper::Request<hyper::body::Incoming>| {
        let response = crate::http::busy(request.uri());
        async move { Ok::<_, std::convert::Infallible>(response) }
    });
    let served = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(REFUSAL_HEAD_TIMEOUT)
        .max_buf_size(MAX_HEAD_BYTES)
        .keep_alive(false)
        .serve_connection(TokioIo::new(io), service);
    match tokio::time::timeout(REFUSAL_LIFETIME, served).await {
        Ok(Err(error)) => {
            tracing::debug!(%error, "a refused remote connection ended with an error");
        }
        Err(_) => tracing::debug!("a refused remote connection outlived its deadline"),
        Ok(Ok(())) => {}
    }
    drop(permit);
}

async fn drain(connections: &mut tokio::task::JoinSet<()>) {
    while connections.join_next().await.is_some() {}
}

/// Serve one connection's requests; an upgraded `/ws` continues on its own task.
async fn serve(
    shared: Arc<Shared>,
    stream: TcpStream,
    bound: SocketAddr,
    mut stopped: watch::Receiver<bool>,
) {
    let service = service_fn(|request| {
        let shared = shared.clone();
        async move {
            Ok::<_, std::convert::Infallible>(crate::http::respond(&shared, bound, request).await)
        }
    });
    let serving = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEAD_TIMEOUT)
        .max_buf_size(MAX_HEAD_BYTES)
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    tokio::pin!(serving);
    let served = tokio::select! {
        served = serving.as_mut() => served,
        _ = stopped.changed() => {
            serving.as_mut().graceful_shutdown();
            serving.await
        }
    };
    if let Err(error) = served {
        tracing::debug!(%error, "a remote connection ended with an error");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    /// A refusal's answer outlasts both buffers of a 64-byte pipe the client
    /// never reads, so the write is still pending when the head timeout has
    /// long passed: only the outer lifetime can end it.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_whose_answer_is_never_read_is_closed_at_its_lifetime() {
        let (server, mut client) = tokio::io::duplex(64);
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: archon.test\r\n\r\n")
            .await
            .unwrap();
        let refusals = Arc::new(Semaphore::new(1));
        let permit = refusals.clone().try_acquire_owned().unwrap();
        let started = tokio::time::Instant::now();
        let bound = Duration::from_secs(6);
        tokio::time::timeout(bound, answer_refusal(server, permit))
            .await
            .expect("a never-read refusal is closed by its lifetime");
        assert_eq!(started.elapsed(), REFUSAL_LIFETIME, "closed at the deadline");
        assert!(REFUSAL_LIFETIME > REFUSAL_HEAD_TIMEOUT, "the head was not what ended it");
        assert_eq!(refusals.available_permits(), 1, "the permit came back");
        let mut written = Vec::new();
        client.read_to_end(&mut written).await.unwrap();
        assert!(written.starts_with(b"HTTP/1.1 503"), "{written:?}");
        assert!(written.len() <= 64, "only what the pipe held: the write was pending");
    }
}
