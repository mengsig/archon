//! The TCP accept loop: plain HTTP/1.1 on loopback, at most
//! `max_connections` connections served at once, heads read within
//! [`HEAD_TIMEOUT`]. TLS is the job of `tailscale serve` or a proxy.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use crate::Shared;

pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
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
        // Full: the newcomer is dropped, and the browser retries.
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::debug!("every remote connection slot is taken; dropping a connection");
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
