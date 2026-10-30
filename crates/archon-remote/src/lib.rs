//! archond's remote listener (docs/remote.md): the embedded mobile web app,
//! one-time pairing, per-device credentials, and a WebSocket that carries
//! archond's own protocol. Off until the operator enables it, bound to
//! loopback, and published over HTTPS by `tailscale serve` or a proxy.
//!
//! A paired device gets no authority of its own: [`allow::allowed`] picks
//! the requests it may send, and [`Backend`] hands each to archond exactly
//! as the operator's CLI would.

pub mod allow;
mod assets;
pub mod devices;
mod http;
mod listener;
pub mod pairing;
mod sessions;
pub mod store;
mod ws;

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use archon_core::error::ArchonError;
use archon_core::protocol::{Reply, Request, ServerFrame};
use archon_core::remote::{DeviceView, PairingCode, RemoteChange, RemoteConfig, RemoteView};
use tokio::sync::mpsc;

use crate::devices::Device;
use crate::pairing::{GuessLimit, Pairing};
use crate::sessions::{Close, Sessions};
use crate::store::Stored;

/// How archond serves what a device sends.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Answer one allowed request with the operator CLI's standing.
    async fn handle(&self, request: Request) -> Result<Reply, ArchonError>;
    /// Send `subscribe_feed`'s reply and then its feed frames to `frames`
    /// until it closes, as the Unix socket does.
    async fn feed(&self, subscription: FeedSubscription, frames: mpsc::Sender<ServerFrame>);
}

/// One `subscribe_feed` on a device's WebSocket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedSubscription {
    /// Numbered per connection from 1, as on the socket.
    pub id: u64,
    /// The request's `req`, for the `subscribed` reply.
    pub request: u64,
    pub after: Option<u64>,
    pub verbose: bool,
}

/// The remote listener and its state; one per archond.
pub struct Remote {
    shared: Arc<Shared>,
    /// Serializes enable, disable and shutdown, which await the listener.
    admin: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("address", &self.shared.config.address)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Shared {
    pub config: RemoteConfig,
    pub path: std::path::PathBuf,
    pub daemon_version: String,
    pub state: Mutex<State>,
    pub backend: OnceLock<Arc<dyn Backend>>,
    pub sessions: Arc<Sessions>,
}

pub(crate) struct State {
    pub stored: Stored,
    pub pairing: Option<Pairing>,
    pub guesses: GuessLimit,
    pub listener: Option<listener::Running>,
    pub error: Option<String>,
}

impl Shared {
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record that device `id` connected; a failed write only loses the time.
    pub fn note_seen(&self, id: &str) {
        let now = archon_core::now();
        let noted = self.lock().update(&self.path, |stored| {
            if let Some(device) = stored.devices.iter_mut().find(|d| d.id == id) {
                device.last_seen_at = Some(now);
            }
            Ok(())
        });
        if let Err(error) = noted {
            tracing::warn!(%error, device = %id, "the device's last connection time was not saved");
        }
    }

    /// Whether a session of device `id` may stay open: the listener is still
    /// on and the device still paired. Revoke and disable change both first.
    pub fn admits(&self, id: &str) -> bool {
        let state = self.lock();
        state.listener.is_some() && state.stored.devices.iter().any(|d| d.id == id)
    }

    /// Forget device `id` and close its sessions at once.
    pub fn revoke(&self, id: &str) -> Result<(), ArchonError> {
        self.lock().update(&self.path, |stored| {
            let before = stored.devices.len();
            stored.devices.retain(|d| d.id != id);
            if stored.devices.len() == before {
                return Err(ArchonError::NotFound(format!("device {id} is not paired")));
            }
            Ok(())
        })?;
        self.sessions.close_device(id, Close::Revoked);
        tracing::info!(device = %id, "a remote device was revoked");
        Ok(())
    }
}

impl Remote {
    /// Load `remote.json` from `data_dir`. Nothing listens until [`Self::restore`].
    pub fn open(
        config: RemoteConfig,
        data_dir: &Path,
        daemon_version: &str,
    ) -> Result<Self, ArchonError> {
        config.validate()?;
        let path = data_dir.join(store::FILE_NAME);
        let stored = store::load(&path)?;
        let state = State {
            stored,
            pairing: None,
            guesses: GuessLimit::default(),
            listener: None,
            error: None,
        };
        let sessions = Arc::new(Sessions::new(config.max_connections as usize));
        let shared = Shared {
            config,
            path,
            daemon_version: daemon_version.to_owned(),
            state: Mutex::new(state),
            backend: OnceLock::new(),
            sessions,
        };
        Ok(Self {
            shared: Arc::new(shared),
            admin: tokio::sync::Mutex::new(()),
        })
    }

    /// Give the listener archond's dispatcher; once, before [`Self::restore`].
    pub fn attach(&self, backend: Arc<dyn Backend>) {
        let fresh = self.shared.backend.set(backend).is_ok();
        assert!(fresh, "the remote backend is attached once");
    }

    /// Open the listener when the stored settings say it is enabled. A bind
    /// failure is kept for `remote_status` and does not stop archond.
    pub async fn restore(&self) {
        let _admin = self.admin.lock().await;
        if self.shared.lock().stored.enabled {
            self.open_listener();
        }
    }

    pub fn view(&self) -> RemoteView {
        let state = self.shared.lock();
        let now = Instant::now();
        let devices = state
            .stored
            .devices
            .iter()
            .map(|d| device_view(d, self.shared.sessions.count(&d.id)))
            .collect();
        RemoteView {
            enabled: state.stored.enabled,
            address: self.shared.config.address.clone(),
            listening: state.listener.as_ref().map(|l| l.address.to_string()),
            error: state.error.clone(),
            public_origins: state.stored.public_origins.clone(),
            devices,
            pairing_expires_at: state
                .pairing
                .as_ref()
                .filter(|p| !p.expired(now))
                .map(Pairing::expires_at),
        }
    }

    /// Apply `change`: the allowlist first, then open or close the listener.
    /// Origins are checked at the upgrade, so withdrawing one closes every session.
    pub async fn configure(&self, change: RemoteChange) -> Result<RemoteView, ArchonError> {
        let _admin = self.admin.lock().await;
        let enable = change.enabled;
        let before = self.shared.lock().stored.public_origins.clone();
        self.update_stored(|stored| apply_origins(stored, &change))?;
        let after = self.shared.lock().stored.public_origins.clone();
        if before.iter().any(|origin| !after.contains(origin)) {
            tracing::info!("an allowed origin was withdrawn; closing remote sessions");
            self.shared.sessions.close_all(Close::OriginWithdrawn);
        }
        match enable {
            Some(true) => {
                self.update_stored(|stored| {
                    stored.enabled = true;
                    Ok(())
                })?;
                if self.shared.lock().listener.is_none() {
                    self.open_listener();
                }
            }
            Some(false) => {
                self.update_stored(|stored| {
                    stored.enabled = false;
                    Ok(())
                })?;
                self.close_listener(Close::Disabled).await;
                self.shared.lock().pairing = None;
            }
            None => {}
        }
        Ok(self.view())
    }

    /// Open a pairing, replacing any open one. Only while listening.
    pub fn pair(&self) -> Result<PairingCode, ArchonError> {
        let mut state = self.shared.lock();
        let Some(listening) = state.listener.as_ref().map(|l| l.address) else {
            return Err(ArchonError::Invalid(
                "remote access is off; run `archon remote setup` or `archon remote enable` first"
                    .into(),
            ));
        };
        if state.stored.devices.len() >= devices::MAX_DEVICES {
            return Err(ArchonError::Invalid(format!(
                "{} devices are paired, the most there may be; revoke one first",
                devices::MAX_DEVICES
            )));
        }
        let pairing = Pairing::open(Instant::now())?;
        let mut urls: Vec<String> = state
            .stored
            .public_origins
            .iter()
            .map(|origin| format!("{origin}/pair"))
            .collect();
        urls.push(format!(
            "{}/pair",
            archon_core::remote::local_origin(listening)
        ));
        let code = PairingCode {
            code: pairing.display_code(),
            expires_at: pairing.expires_at(),
            urls,
        };
        state.pairing = Some(pairing);
        tracing::info!("a remote pairing was opened");
        Ok(code)
    }

    /// Unpair the device with this id or name; its sessions close at once.
    pub fn revoke(&self, device: &str) -> Result<RemoteView, ArchonError> {
        let id = {
            let state = self.shared.lock();
            resolve_device(&state.stored.devices, device)?
        };
        self.shared.revoke(&id)?;
        Ok(self.view())
    }

    /// Close every connection as archond stops.
    pub async fn shutdown(&self) {
        let _admin = self.admin.lock().await;
        self.close_listener(Close::ShuttingDown).await;
    }

    fn open_listener(&self) {
        let opened = listener::start(self.shared.clone());
        let mut state = self.shared.lock();
        match opened {
            Ok(running) => {
                tracing::info!(address = %running.address, "remote listener open");
                state.listener = Some(running);
                state.error = None;
            }
            Err(error) => {
                tracing::error!(%error, "the remote listener did not open");
                state.error = Some(error.to_string());
            }
        }
    }

    async fn close_listener(&self, why: Close) {
        // Taken before the broadcast, so an upgrade racing it is refused.
        let running = self.shared.lock().listener.take();
        self.shared.sessions.close_all(why);
        if let Some(running) = running {
            running.stop().await;
            tracing::info!("remote listener closed");
        }
    }

    fn update_stored(
        &self,
        change: impl FnOnce(&mut Stored) -> Result<(), ArchonError>,
    ) -> Result<(), ArchonError> {
        self.shared.lock().update(&self.shared.path, change)
    }
}

impl State {
    /// Change the stored state and write it; on a failed write nothing changes.
    pub fn update(
        &mut self,
        path: &Path,
        change: impl FnOnce(&mut Stored) -> Result<(), ArchonError>,
    ) -> Result<(), ArchonError> {
        let mut next = self.stored.clone();
        change(&mut next)?;
        if next == self.stored {
            return Ok(());
        }
        store::save(path, &next)?;
        self.stored = next;
        Ok(())
    }
}

fn device_view(device: &Device, sessions: u32) -> DeviceView {
    DeviceView {
        id: device.id.clone(),
        name: device.name.clone(),
        paired_at: device.paired_at,
        last_seen_at: device.last_seen_at,
        sessions,
    }
}

fn apply_origins(stored: &mut Stored, change: &RemoteChange) -> Result<(), ArchonError> {
    let add: Vec<String> = change
        .add_origins
        .iter()
        .map(|o| archon_core::remote::normalize_origin(o))
        .collect::<Result<_, _>>()?;
    let remove: Vec<String> = change
        .remove_origins
        .iter()
        .map(|o| archon_core::remote::normalize_origin(o))
        .collect::<Result<_, _>>()?;
    stored.public_origins.retain(|o| !remove.contains(o));
    for origin in add {
        if !stored.public_origins.contains(&origin) {
            stored.public_origins.push(origin);
        }
    }
    if stored.public_origins.len() > archon_core::remote::MAX_PUBLIC_ORIGINS {
        return Err(ArchonError::Invalid(format!(
            "at most {} public origins; remove one first",
            archon_core::remote::MAX_PUBLIC_ORIGINS
        )));
    }
    Ok(())
}

/// A device by exact id, or by a name only one device has.
fn resolve_device(devices: &[Device], wanted: &str) -> Result<String, ArchonError> {
    if let Some(device) = devices.iter().find(|d| d.id == wanted) {
        return Ok(device.id.clone());
    }
    let named: Vec<&Device> = devices.iter().filter(|d| d.name == wanted).collect();
    match named.as_slice() {
        [device] => Ok(device.id.clone()),
        [] => Err(ArchonError::NotFound(format!(
            "no paired device has the id or name {wanted:?}; `archon remote devices` lists them"
        ))),
        _ => Err(ArchonError::Invalid(format!(
            "{} devices are named {wanted:?}; revoke one by its id",
            named.len()
        ))),
    }
}
