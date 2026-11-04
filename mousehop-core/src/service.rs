use crate::{
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    clipboard_bulk::{OutgoingPayload, OutgoingTransfer},
    config::{Config, ConfigClient},
    connect::MousehopConnection,
    crypto,
    discovery::{Discovery, DiscoveryEvent, PrimaryCache},
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    file_paste::{ActivePaste, OutgoingFile, PendingFileConflict, RemoteFileOffer},
    latency::{LatencyProber, ProbeResult},
    listen::{ListenerCreationError, ListenerSession, MousehopListener},
    network::current_network_id,
};
use futures::StreamExt;
use input_capture::clipboard::{ClipboardMonitor, SuppressionList, clipboard_signature};
use input_event::{ClipboardEvent, Event as InputEvent};
use local_channel::mpsc::{Receiver, channel};
use log;
use mousehop_ipc::{
    AppIdent, AsyncFrontendListener, ClientHandle, ConnectionMode, FilePastePhase,
    FilePasteProgress, FrontendEvent, FrontendRequest, HostKind, IncomingPeerConfig, IpcError,
    IpcListenerCreationError, Position, RemoteHostState, Status,
};
use mousehop_proto::{
    CAP_CLIPBOARD_FILES, CAP_CLIPBOARD_IMAGE, CAP_CLIPBOARD_V2, CAP_EXPLICIT_FILE_PASTE,
    MAX_CLIPBOARD_SIZE, ProtoEvent,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        if mousehop_ipc::desktop::current().is_some() {
            crate::windows_service::session_shutdown().await
        } else if crate::windows_service::worker_mode() {
            crate::windows_service::worker_shutdown().await
        } else {
            signal::ctrl_c().await
        }
    }
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
}

type DeferredClipboard = (ClipboardEvent, String, Instant);

fn take_ready_clipboards(
    pending: &mut HashMap<ClientHandle, DeferredClipboard>,
    enabled: &HashSet<ClientHandle>,
    is_ready: impl Fn(ClientHandle) -> bool,
) -> Vec<(ClientHandle, DeferredClipboard)> {
    let mut ready = Vec::new();
    pending.retain(|handle, (_, _, queued)| {
        if !enabled.contains(handle) || queued.elapsed() >= Duration::from_secs(15) {
            log::debug!("discarding deferred clipboard for client {handle}: disabled or connection timed out");
            return false;
        }
        if is_ready(*handle) { ready.push(*handle); }
        true
    });
    ready
        .into_iter()
        .filter_map(|handle| pending.remove(&handle).map(|event| (handle, event)))
        .collect()
}

struct PreparedClipboard {
    cancel: tokio_util::sync::CancellationToken,
    targets: Vec<ClientHandle>,
    origin: String,
    version: Option<u64>,
    is_file: bool,
    capability: u32,
    result: Result<OutgoingPayload, crate::clipboard_bulk::BulkTransportError>,
}

impl PreparedClipboard {
    fn can_publish(&self, sharing: bool, desktop: bool, version: Option<u64>) -> bool {
        !self.cancel.is_cancelled()
            && sharing
            && desktop
            && (!self.is_file || version == self.version)
    }
}

struct ClipboardPreparations {
    jobs: tokio::task::JoinSet<PreparedClipboard>,
    cancel: tokio_util::sync::CancellationToken,
}
impl Default for ClipboardPreparations {
    fn default() -> Self {
        Self {
            jobs: tokio::task::JoinSet::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }
}
impl ClipboardPreparations {
    fn cancel(&mut self) {
        self.cancel.cancel();
        self.cancel = tokio_util::sync::CancellationToken::new();
        self.jobs.abort_all();
    }
}
impl Drop for ClipboardPreparations {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub struct Service {
    #[cfg(target_os = "macos")]
    _permission_watcher: Option<crate::macos_tcc_watch::PermissionWatcher>,
    permission_revocations: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    permissions_revoked: bool,
    sharing: bool,
    stopped: bool,
    exit_requested: bool,
    desktop_agent_seen: Option<Instant>,
    desktop_available: bool,
    pending_pairing: HashSet<String>,
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// background per-address latency prober. Feeds the GUI's
    /// per-candidate RTT display so the user can compare interfaces on
    /// a multi-homed peer before locking one.
    latency: LatencyProber,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, IncomingPeerConfig>>>,
    /// Shared mDNS browse cache. Used at DTLS-accept time to
    /// reverse-lookup the connecting peer's IP back to the system
    /// hostname they advertise via Bonjour, so the GUI can show a
    /// human-readable identity in the Incoming Connections list.
    primary_cache: PrimaryCache,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    /// Retained peer lock-recovery states, keyed by authenticated certificate
    /// fingerprint so a reconnect on a new source port keeps the same identity.
    remote_host_states: HashMap<String, RemoteHostRecovery>,
    next_trigger_handle: u64,
    /// mDNS-SD service registration + browse. Advertises our primary
    /// interface IP for peer dialers to bias toward; populates
    /// shared `PrimaryCache` (read by `MousehopConnection`) from
    /// peer announcements.
    discovery: Discovery,
    /// Stream of resolved peer advertisements from [`Discovery`]'s
    /// browse task — folded into client candidate sets + interface
    /// labels in the main loop.
    discovery_event_rx: Receiver<DiscoveryEvent>,
    /// Opaque fingerprint of the network the machine is currently on
    /// (default-gateway MAC, or subnet fallback). Drives per-network
    /// lock resolution; recomputed periodically and on interface
    /// changes. `None` when undeterminable (locks then don't apply).
    current_network: Option<String>,
    /// Outgoing connection handle to fan clipboard frames out from
    /// the capture / forwarding paths. Same handle Capture owns;
    /// cloned in `Service::new` so Service can call `send` directly
    /// without routing through the capture session loop.
    conn: MousehopConnection,
    /// Cross-platform clipboard poller. `None` when the platform
    /// clipboard couldn't be opened (headless CI, Wayland session
    /// without compositor support). Service drains it in the main
    /// loop and fans the resulting events out to peers whose
    /// `clipboard_send` is true.
    clipboard_monitor: Option<ClipboardMonitor>,
    desktop_clipboard: Option<tokio::sync::mpsc::Receiver<(u64, ClipboardEvent)>>,
    /// Recent forwards keyed on `(originator_fingerprint, hash)`.
    /// Used to break N-peer rebroadcast cycles: when this device
    /// receives a forwarded clipboard frame and would re-fan to
    /// other peers, the entry under (origin, content_hash) blocks
    /// the duplicate. Pruned lazily — entries older than
    /// `RECENT_FORWARD_TTL` are dropped on each clipboard event.
    recent_forwarded: HashMap<(String, u64), Instant>,
    // Latest clipboard per peer while its authenticated outbound session connects.
    pending_clipboards: HashMap<ClientHandle, DeferredClipboard>,
    file_senders: HashMap<ClientHandle, OutgoingFile>,
    clipboard_preparations: ClipboardPreparations,
    file_offer: Option<RemoteFileOffer>,
    active_paste: Option<ActivePaste>,
    file_progress_tx: tokio::sync::mpsc::UnboundedSender<FilePasteProgress>,
    file_progress_rx: tokio::sync::mpsc::UnboundedReceiver<FilePasteProgress>,
    last_file_progress: Option<FilePasteProgress>,
    file_conflict_tx: tokio::sync::mpsc::UnboundedSender<PendingFileConflict>,
    file_conflict_rx: tokio::sync::mpsc::UnboundedReceiver<PendingFileConflict>,
    clipboard_version: Option<u64>,
    native_arming: Option<([u8; 16], Instant)>,
    /// Shared with [`ClipboardMonitor`]; mutations to the inner
    /// `HashSet` take effect on the next clipboard poll without
    /// rebuilding the monitor.
    clipboard_suppression: SuppressionList,
    /// Per-client count of consecutive probe observations where a
    /// substantially-faster candidate beat the active path, gating the
    /// `Fastest`-mode auto-upgrade so noise can't trigger a switch.
    fastest_streak: HashMap<ClientHandle, u8>,
}

/// Consecutive probe observations a faster candidate must win before
/// `Fastest` mode actually switches to it. With the ~5 s probe cadence
/// this is ~15 s of sustained advantage — long enough that a transient
/// latency dip never causes a flap.
const FASTEST_UPGRADE_STREAK: u8 = 3;

const RECENT_FORWARD_TTL: Duration = Duration::from_secs(1);

#[cfg(test)]
fn clipboard_hash(content: &str) -> u64 {
    clipboard_signature(&ClipboardEvent::Text(content.into()))
}

fn clipboard_log_label(event: &ClipboardEvent) -> String {
    match event {
        ClipboardEvent::Text(text) => format!("clipboard text ({} bytes)", text.len()),
        ClipboardEvent::ImageRgba { width, height, .. } => {
            format!("clipboard image ({width}x{height})")
        }
        ClipboardEvent::Files { paths, .. } => {
            format!("clipboard file list ({} item(s))", paths.len())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(test)]
enum ClipboardTransport {
    Legacy,
    Bulk,
    Unsupported,
}

#[cfg(test)]
fn clipboard_transport(encoded_size: usize, peer_capabilities: u32) -> ClipboardTransport {
    if encoded_size <= MAX_CLIPBOARD_SIZE {
        ClipboardTransport::Legacy
    } else if peer_capabilities & CAP_CLIPBOARD_V2 != 0 {
        ClipboardTransport::Bulk
    } else {
        ClipboardTransport::Unsupported
    }
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    session: ListenerSession,
    pos: Position,
}

#[derive(Clone, Debug)]
struct RemoteHostRecovery {
    addr: SocketAddr,
    state: RemoteHostState,
}

fn mark_locked_remote_host_unavailable(
    states: &mut HashMap<String, RemoteHostRecovery>,
    addr: SocketAddr,
) -> Option<(String, SocketAddr)> {
    states
        .iter_mut()
        .find(|(_, recovery)| recovery.addr == addr && recovery.state == RemoteHostState::Locked)
        .map(|(fingerprint, recovery)| {
            recovery.state = RemoteHostState::Unavailable;
            (fingerprint.clone(), recovery.addr)
        })
}

fn record_remote_host_state(
    states: &mut HashMap<String, RemoteHostRecovery>,
    fingerprint: &str,
    addr: SocketAddr,
    state: RemoteHostState,
) -> bool {
    if state == RemoteHostState::Unlocked {
        return states.remove(fingerprint).is_some();
    }
    let changed = states
        .get(fingerprint)
        .is_none_or(|recovery| recovery.addr != addr || recovery.state != state);
    states.insert(fingerprint.to_owned(), RemoteHostRecovery { addr, state });
    changed
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        // listener + connection. The primary-IP cache is owned by
        // the dialer side so its references survive Discovery
        // toggles; Discovery writes peer hints into it as browse
        // events arrive.
        let listener =
            MousehopListener::new(config.port(), cert.clone(), authorized_keys.clone()).await?;
        let primary_cache: PrimaryCache = Default::default();
        let conn =
            MousehopConnection::new(cert.clone(), client_manager.clone(), primary_cache.clone());

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let conn_for_service = conn.sender_clone();
        let capture = Capture::new(
            capture_backend,
            conn,
            config.release_bind(),
            config.release_threshold_px(),
        );
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);
        // Push the persisted authorized-peers table into the receive
        // pipeline so per-peer post-processing is applied from the
        // first incoming packet.
        emulation.set_incoming_peers(authorized_keys.read().expect("lock").clone());

        // create dns resolver
        let resolver = DnsResolver::new()?;

        // background latency prober — pulls candidate addresses from
        // the (shared) client manager itself, so no targets need to be
        // pushed in as clients/DNS change.
        let latency = LatencyProber::new(client_manager.clone());

        let port = config.port();
        let (discovery_event_tx, discovery_event_rx) = channel();
        let discovery = Discovery::new(
            port,
            config.mdns_discovery(),
            primary_cache.clone(),
            discovery_event_tx,
        );
        // Resolve the lock (if any) for the network we boot on, before
        // the first dial, so a locked client connects on the right
        // interface immediately.
        let current_network = current_network_id();
        for handle in client_manager.registered_clients() {
            client_manager.recompute_active_lock(handle, current_network.as_deref());
        }
        // ClipboardMonitor is best-effort: a headless CI environment
        // or a Wayland session without compositor support yields a
        // permanent error here. We log and proceed without clipboard
        // sync rather than tying daemon startup to clipboard
        // availability.
        let clipboard_suppression: SuppressionList = {
            let host = HostKind::current();
            let initial: HashSet<AppIdent> = config
                .clipboard_suppression()
                .host()
                .iter()
                .cloned()
                .map(|s| host.make_ident(s))
                .collect();
            Arc::new(std::sync::Mutex::new(initial))
        };
        let allow_clipboard = {
            #[cfg(windows)]
            {
                mousehop_ipc::desktop::current().is_none()
                    && !crate::windows_service::secure_desktop()
            }
            #[cfg(not(windows))]
            {
                true
            }
        };
        let clipboard_monitor = if allow_clipboard {
            match ClipboardMonitor::with_suppression(clipboard_suppression.clone()) {
                Ok(m) => Some(m),
                Err(e) => {
                    log::warn!("clipboard monitor unavailable: {e}; clipboard sync disabled");
                    None
                }
            }
        } else {
            None
        };
        let (file_progress_tx, file_progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let (file_conflict_tx, file_conflict_rx) = tokio::sync::mpsc::unbounded_channel();
        #[cfg(windows)]
        let state = crate::windows_service::runtime_state()?;
        #[cfg(not(windows))]
        let state = mousehop_ipc::RuntimeState::Running;
        #[cfg(target_os = "macos")]
        let (permission_watcher, permission_revocations) = match crate::macos_tcc_watch::spawn() {
            Some((watcher, changes)) => (Some(watcher), Some(changes)),
            None => (None, None),
        };
        #[cfg(not(target_os = "macos"))]
        let permission_revocations = None;
        let service = Self {
            #[cfg(target_os = "macos")]
            _permission_watcher: permission_watcher,
            permission_revocations,
            permissions_revoked: false,
            sharing: state == mousehop_ipc::RuntimeState::Running,
            stopped: state == mousehop_ipc::RuntimeState::Stopped,
            exit_requested: false,
            desktop_agent_seen: None,
            desktop_available: true,
            pending_pairing: HashSet::new(),
            config,
            capture,
            emulation,
            frontend_listener,
            resolver,
            latency,
            authorized_keys,
            primary_cache,
            public_key_fingerprint,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            remote_host_states: Default::default(),
            next_trigger_handle: 0,
            discovery,
            discovery_event_rx,
            current_network,
            conn: conn_for_service,
            clipboard_monitor,
            #[cfg(windows)]
            desktop_clipboard: mousehop_ipc::desktop::current()
                .map(|link| link.subscribe_clipboard()),
            #[cfg(not(windows))]
            desktop_clipboard: None,
            recent_forwarded: HashMap::new(),
            pending_clipboards: HashMap::new(),
            file_senders: HashMap::new(),
            clipboard_preparations: ClipboardPreparations::default(),
            file_offer: None,
            active_paste: None,
            file_progress_tx,
            file_progress_rx,
            last_file_progress: None,
            file_conflict_tx,
            file_conflict_rx,
            clipboard_version: input_capture::clipboard::native_clipboard_version(),
            native_arming: None,
            clipboard_suppression,
            fastest_streak: HashMap::new(),
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        if !self.sharing {
            self.set_sharing(false, self.stopped).await;
        }
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }

        // Periodic refresh of the Discovery service registration so
        // its TXT record stays accurate when the OS-preferred
        // interface (default route) changes — e.g. user switches
        // off Wi-Fi and Mac falls back to Ethernet. Cheap: at most
        // one re-publish every 30s, and a no-op when the primary
        // hasn't moved. `Skip` so a long suspend doesn't backlog-
        // burst on resume.
        let mut discovery_refresh_tick = tokio::time::interval(Duration::from_secs(30));
        discovery_refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // skip the immediate-fire of the first tick — Discovery
        // already published once at startup
        discovery_refresh_tick.tick().await;

        let mut clipboard_retry = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = wait_for_permission_revocation(&mut self.permission_revocations) => {
                    self.permissions_revoked = true;
                    self.set_sharing(false, false).await;
                    self.notify_frontend(FrontendEvent::OpenSettings);
                    self.notify_frontend(FrontendEvent::Error(
                        "macOS input permissions were removed. Grant permissions in System Settings, then restart Mousehop from Settings.".into()
                    ));
                },
                _ = clipboard_retry.tick() => {
                    if self.desktop_agent_seen.is_some_and(|seen| seen.elapsed() > Duration::from_secs(5)) {
                        log::warn!("Desktop Agent heartbeat expired; ending its sharing session");
                        self.desktop_agent_seen = None;
                        self.invalidate_file_transfers().await;
                        #[cfg(windows)]
                        let installed = mousehop_ipc::windows::service_mode();
                        #[cfg(not(windows))]
                        let installed = false;
                        if !installed { break; }
                    }
                    #[cfg(windows)]
                    let available = mousehop_ipc::desktop::current().map_or(true, |link| link.ready() && !link.secure());
                    #[cfg(not(windows))]
                    let available = true;
                    if available != self.desktop_available {
                        self.desktop_available = available;
                        self.notify_frontend(FrontendEvent::DesktopAvailable(available));
                        if !available { self.invalidate_file_transfers().await; }
                    }
                    let version = input_capture::clipboard::native_clipboard_version();
                    if self.native_arming.is_some_and(|(_, started)| started.elapsed() >= Duration::from_secs(5)) {
                        self.invalidate_file_transfers().await;
                    }
                    if version != self.clipboard_version && self.native_arming.is_none() {
                        self.clipboard_version = version;
                        self.invalidate_file_transfers().await;
                    }
                    self.file_senders.retain(|handle, sender| !sender.task.is_finished() && self.client_manager.alive(*handle));
                    if !self.pending_clipboards.is_empty() { self.retry_pending_clipboards().await; }
                },
                Some(progress) = self.file_progress_rx.recv() => self.file_paste_progress(progress),
                Some(conflict) = self.file_conflict_rx.recv() => self.file_paste_conflict(conflict),
                Some(result) = self.clipboard_preparations.jobs.join_next(), if !self.clipboard_preparations.jobs.is_empty() => {
                    match result {
                        Ok(prepared) => self.clipboard_prepared(prepared).await,
                        Err(error) if error.is_cancelled() => {},
                        Err(error) => log::error!("Clipboard preparation task failed: {error}"),
                    }
                },
                request = self.frontend_listener.next() => {
                    self.handle_frontend_request(request).await;
                    if self.exit_requested { break; }
                },
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event).await,
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                result = self.latency.event() => self.handle_probe_result(result).await,
                event = recv_discovery(&mut self.discovery_event_rx) => {
                    self.handle_discovery_event(event)
                }
                _ = self.config.changed() => self.handle_config_change().await,
                _ = discovery_refresh_tick.tick() => self.on_discovery_refresh().await,
                event = recv_clipboard(&mut self.clipboard_monitor) => {
                    self.handle_local_clipboard_event(event).await;
                }
                event = recv_desktop_clipboard(&mut self.desktop_clipboard) => {
                    self.handle_local_clipboard_event(Some(input_capture::CaptureEvent::Input(InputEvent::Clipboard(event)))).await;
                }
                result = shutdown_signal() => break result?,
            }
        }

        log::info!("terminating service ...");
        self.handle_frontend_pending().await;
        self.frontend_listener.flush().await;
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;
        log::debug!("terminating latency prober ...");
        self.latency.terminate().await;

        Ok(())
    }

    async fn set_sharing(&mut self, enabled: bool, stopped: bool) {
        if enabled && self.permissions_revoked {
            self.notify_frontend(FrontendEvent::Error(
                "Restart Mousehop after granting macOS input permissions.".into(),
            ));
            return;
        }
        self.sharing = enabled;
        self.stopped = stopped;
        #[cfg(windows)]
        if let Err(error) = crate::windows_service::save_runtime_state(if stopped {
            mousehop_ipc::RuntimeState::Stopped
        } else if enabled {
            mousehop_ipc::RuntimeState::Running
        } else {
            mousehop_ipc::RuntimeState::Paused
        }) {
            log::error!("Persist runtime intent: {error}");
        }
        if enabled {
            self.capture.reenable();
        } else {
            self.capture.pause().await;
            self.invalidate_file_transfers().await;
            self.pending_clipboards.clear();
        }
        self.emulation.set_paused(!enabled).await;
        self.notify_frontend(FrontendEvent::RuntimeState(if stopped {
            mousehop_ipc::RuntimeState::Stopped
        } else if enabled {
            mousehop_ipc::RuntimeState::Running
        } else {
            mousehop_ipc::RuntimeState::Paused
        }));
    }

    async fn handle_frontend_request(
        &mut self,
        request: Option<Result<FrontendRequest, IpcError>>,
    ) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        if self.permissions_revoked
            && matches!(
                request,
                FrontendRequest::EnableCapture | FrontendRequest::EnableEmulation
            )
        {
            self.notify_frontend(FrontendEvent::Error(
                "Restart Mousehop after granting macOS input permissions.".into(),
            ));
            return;
        }
        match request {
            FrontendRequest::Hello { .. } => {}
            FrontendRequest::SetAutostart(enabled) => {
                self.notify_frontend(FrontendEvent::ConfigureAutostart(enabled))
            }
            FrontendRequest::AutostartReport {
                enabled,
                managed,
                error,
            } => self.notify_frontend(FrontendEvent::Autostart {
                enabled,
                managed,
                error,
            }),
            FrontendRequest::SetSharing(enabled) => self.set_sharing(enabled, false).await,
            FrontendRequest::Quit => {
                log::info!("Application quit requested through local IPC");
                self.set_sharing(false, true).await;
                // Installed Windows keeps the service alive. Notify Settings
                // immediately in both modes, before accepting another request.
                self.handle_frontend_pending().await;
                #[cfg(windows)]
                let installed = mousehop_ipc::windows::service_mode();
                #[cfg(not(windows))]
                let installed = false;
                self.exit_requested = !installed;
            }
            FrontendRequest::RunningAppsReport(apps) => {
                self.notify_frontend(FrontendEvent::RunningApps(apps))
            }
            FrontendRequest::Restart => self.notify_frontend(FrontendEvent::Restart),
            FrontendRequest::OpenSettings => self.notify_frontend(FrontendEvent::OpenSettings),
            FrontendRequest::DesktopAgentConnected { sharing } => {
                self.invalidate_file_transfers().await;
                self.desktop_agent_seen = Some(Instant::now());
                self.set_sharing(sharing, false).await;
                self.sync_frontend();
                self.notify_frontend(FrontendEvent::AgentAttached(if sharing {
                    mousehop_ipc::RuntimeState::Running
                } else {
                    mousehop_ipc::RuntimeState::Paused
                }));
            }
            FrontendRequest::DesktopAgentHeartbeat => {
                self.desktop_agent_seen = Some(Instant::now())
            }
            FrontendRequest::DesktopAgentDisconnected => {
                self.desktop_agent_seen = None;
                self.invalidate_file_transfers().await;
            }
            FrontendRequest::PrepareNativeFilePaste { id } => {
                if !self.sharing || !self.desktop_available || self.desktop_agent_seen.is_none() {
                    return;
                }
                if let Some(offer) = self
                    .file_offer
                    .as_ref()
                    .filter(|offer| offer.stream.offer.transfer_id.0 == id)
                {
                    let version = offer.clipboard_version;
                    self.native_arming = Some((id, Instant::now()));
                    self.notify_frontend(FrontendEvent::PrepareNativeFilePaste {
                        id,
                        clipboard_version: version,
                    });
                }
            }
            FrontendRequest::NativeFilePastePrepared {
                id,
                clipboard_version,
                error,
            } => {
                if self
                    .file_offer
                    .as_ref()
                    .is_some_and(|offer| offer.stream.offer.transfer_id.0 == id)
                {
                    // Installed Windows workers cannot inspect the user's OLE
                    // clipboard. The user-process bridge validates its marker;
                    // acknowledge only the outstanding preparation and generation.
                    let valid = error.is_none()
                        && self.native_arming.is_some_and(|(arming, _)| arming == id)
                        && clipboard_version
                            == input_capture::clipboard::native_clipboard_version();
                    self.native_arming = None;
                    if valid {
                        self.clipboard_version = clipboard_version;
                        if let Some(offer) = &mut self.file_offer {
                            offer.clipboard_version = clipboard_version;
                        }
                    } else {
                        self.fail_native_paste(
                            id,
                            error.unwrap_or_else(|| {
                                "Cannot prepare the native Paste command".into()
                            }),
                        );
                    }
                }
            }
            FrontendRequest::NativeFilePasteFailed { id, message } => {
                self.fail_native_paste(id, message)
            }
            FrontendRequest::PasteFiles { id, destination } => {
                self.start_file_paste(id, destination)
            }
            FrontendRequest::ResolveFilePasteConflict {
                id,
                request_id,
                action,
            } => {
                let resume_saving =
                    !matches!(action, mousehop_ipc::FilePasteConflictAction::Cancel);
                let mut resolved = false;
                if let Some(active) = self.active_paste.as_mut().filter(|paste| paste.id == id) {
                    if active
                        .conflict
                        .as_ref()
                        .is_some_and(|pending| pending.question.request_id == request_id)
                    {
                        if let Some(pending) = active.conflict.take() {
                            resolved = pending.reply.send(action).is_ok();
                        }
                    }
                }
                if resolved && resume_saving {
                    if let Some(mut progress) = self
                        .last_file_progress
                        .clone()
                        .filter(|progress| progress.id == id)
                    {
                        progress.phase = FilePastePhase::Saving;
                        self.file_paste_progress(progress);
                    }
                }
            }
            FrontendRequest::CancelFilePaste { id } => {
                if self
                    .active_paste
                    .as_ref()
                    .is_some_and(|paste| paste.id == id)
                {
                    self.cancel_file_paste();
                }
            }
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config().await;
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.pending_pairing.remove(&fp);
                self.add_authorized_key(desc, fp);
                self.save_config().await;
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config().await;
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle).await;
                self.save_config().await;
            }
            FrontendRequest::EnableCapture => {
                if self.sharing {
                    self.capture.reenable();
                }
            }
            FrontendRequest::EnableEmulation => {
                if self.sharing {
                    self.emulation.reenable();
                }
            }
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.conn.reset_handle(handle).await;
                self.update_fix_ips(handle, fix_ips);
                self.save_config().await;
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.conn.reset_handle(handle).await;
                self.update_hostname(handle, host);
                self.save_config().await;
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.conn.reset_handle(handle).await;
                self.update_port(handle, port);
                self.save_config().await;
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config().await;
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.invalidate_file_transfers().await;
                self.remove_authorized_key(key);
                self.save_config().await;
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook);
                self.save_config().await;
            }
            FrontendRequest::SaveConfiguration => self.save_config().await,
            FrontendRequest::SetReleaseThreshold(threshold) => {
                self.config.set_release_threshold_px(threshold);
                self.capture.set_release_threshold(threshold);
                self.notify_frontend(FrontendEvent::ReleaseThreshold(threshold));
                self.save_config().await;
            }
            FrontendRequest::SetReleaseBind(bind) => {
                self.config.set_release_bind(bind);
                let canonical = self.config.release_bind();
                self.capture.set_release_bind(canonical.clone());
                self.notify_frontend(FrontendEvent::ReleaseBind(canonical));
                self.save_config().await;
            }
            FrontendRequest::SetIncomingPeerNaturalScroll(fp, natural_scroll) => {
                self.set_incoming_peer_natural_scroll(fp, natural_scroll);
                self.save_config().await;
            }
            FrontendRequest::SetIncomingPeerSensitivity(fp, sensitivity) => {
                self.set_incoming_peer_sensitivity(fp, sensitivity);
                self.save_config().await;
            }
            FrontendRequest::SetMdnsDiscovery(enabled) => {
                self.config.set_mdns_discovery(enabled);
                self.discovery.set_enabled(enabled);
                self.notify_frontend(FrontendEvent::MdnsDiscovery(enabled));
                self.save_config().await;
            }
            FrontendRequest::SetClientClipboardSend(handle, enabled) => {
                if !enabled {
                    self.invalidate_file_transfers().await;
                }
                if self.client_manager.set_clipboard_send(handle, enabled) {
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetClientCommandAsCtrl(handle, enabled) => {
                if self.client_manager.set_command_as_ctrl(handle, enabled) {
                    self.capture.set_command_as_ctrl(handle, enabled);
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetClientRequireCrossingModifier(handle, required) => {
                if self
                    .client_manager
                    .set_require_crossing_modifier(handle, required)
                {
                    let modifier = self.client_manager.required_crossing_modifier(handle);
                    self.capture.set_crossing_modifier(handle, modifier);
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetClientCrossingModifier(handle, modifier) => {
                if self.client_manager.set_crossing_modifier(handle, modifier) {
                    let modifier = self.client_manager.required_crossing_modifier(handle);
                    self.capture.set_crossing_modifier(handle, modifier);
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetConnectionMode(handle, mode) => {
                // Resolve the live network first: the user is acting on a
                // connected machine, so a stale cached `None` (from a
                // transient blip) must not leave this network's lock in
                // place to silently re-activate later.
                self.refresh_network().await;
                let net = self.current_network.clone();
                if self.client_manager.set_mode(handle, mode, net.as_deref()) {
                    // The mode change may have cleared this network's
                    // lock; re-resolve and re-dial so the new policy
                    // takes effect immediately.
                    self.client_manager
                        .recompute_active_lock(handle, net.as_deref());
                    self.conn.reset_handle(handle).await;
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetNetworkLock(handle, ip) => {
                // Resolve the live network first (see SetConnectionMode).
                self.refresh_network().await;
                let Some(net) = self.current_network.clone() else {
                    log::warn!("cannot lock client {handle}: current network unknown");
                    return;
                };
                if self.client_manager.set_network_lock(handle, &net, ip) {
                    self.client_manager
                        .recompute_active_lock(handle, Some(&net));
                    // Drop any live connection so the lock takes effect
                    // on the next event instead of waiting for the
                    // current (possibly wrong-interface) path to drop.
                    self.conn.reset_handle(handle).await;
                    self.broadcast_client(handle);
                    self.save_config().await;
                }
            }
            FrontendRequest::SetIncomingPeerClipboardReceive(fp, enabled) => {
                if !enabled {
                    self.invalidate_file_transfers().await;
                }
                self.set_incoming_peer_clipboard_receive(fp, enabled);
                self.save_config().await;
            }
            FrontendRequest::AddSuppressedApp(value) => {
                self.add_suppressed_app(value);
                self.save_config().await;
            }
            FrontendRequest::RemoveSuppressedApp(value) => {
                self.remove_suppressed_app(value);
                self.save_config().await;
            }
            FrontendRequest::ListRunningApps => {
                self.notify_frontend(FrontendEvent::RequestRunningApps);
            }
        }
    }

    fn add_suppressed_app(&mut self, value: String) {
        let value = value.trim().to_owned();
        if value.is_empty() {
            return;
        }
        let mut suppression = self.config.clipboard_suppression();
        let host = suppression.host_mut();
        if !host.iter().any(|v| v.eq_ignore_ascii_case(&value)) {
            host.push(value);
        }
        self.commit_suppression(suppression);
    }

    fn remove_suppressed_app(&mut self, value: String) {
        let mut suppression = self.config.clipboard_suppression();
        suppression
            .host_mut()
            .retain(|v| !v.eq_ignore_ascii_case(&value));
        self.commit_suppression(suppression);
    }

    /// Persist the per-OS struct, refresh the runtime `HashSet`
    /// shared with [`ClipboardMonitor`], and push the host slot to
    /// the GUI. Centralized so add/remove can't drift apart.
    fn commit_suppression(&mut self, suppression: mousehop_ipc::ClipboardSuppression) {
        let host = HostKind::current();
        let host_list = suppression.host().clone();
        {
            let mut guard = self.clipboard_suppression.lock().expect("lock");
            guard.clear();
            for s in &host_list {
                guard.insert(host.make_ident(s.clone()));
            }
        }
        self.config.set_clipboard_suppression(suppression);
        self.notify_frontend(FrontendEvent::SuppressedAppsUpdated(host_list));
    }

    /// Refresh `last_addr` / `last_hostname` for the authorized-peer
    /// entry matching `fingerprint` whenever a DTLS connect lands.
    /// Hostname comes from a reverse-lookup against the mDNS
    /// `hostname → primary_ip` cache; falls through to addr-only
    /// if discovery isn't running on either end or the peer's
    /// announced primary differs from the IP it actually connected
    /// from. Persists the update so the GUI keeps a useful
    /// identification across restarts.
    async fn update_incoming_peer_address(&mut self, addr: SocketAddr, fingerprint: &str) {
        let ip = addr.ip().to_string();
        let hostname = self.lookup_hostname_for_ip(addr.ip());
        // Scope the write guard to this block: `save_config().await`
        // below must not run while the non-async RwLock is held, or
        // the lock would span a suspension point. The block yields
        // the cloned snapshot and drops the guard before the await.
        let snapshot = {
            let mut keys = self.authorized_keys.write().expect("lock");
            let Some(peer) = keys.get_mut(fingerprint) else {
                return; // unauthorized peer; nothing to update
            };
            let mut changed = peer.last_addr.as_deref() != Some(&ip);
            if changed {
                peer.last_addr = Some(ip);
            }
            if let Some(h) = hostname {
                if peer.last_hostname.as_deref() != Some(h.as_str()) {
                    peer.last_hostname = Some(h);
                    changed = true;
                }
            }
            if !changed {
                return;
            }
            keys.clone()
        };
        // No need to push to InputEmulation — last_addr/last_hostname
        // are display-only; per-pair scroll/sensitivity is unaffected.
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(snapshot));
        self.save_config().await;
    }

    fn lookup_hostname_for_ip(&self, target: std::net::IpAddr) -> Option<String> {
        self.primary_cache
            .borrow()
            .iter()
            .find_map(|(host, ip)| (*ip == target).then(|| host.clone()))
    }

    fn set_incoming_peer_natural_scroll(&mut self, fingerprint: String, natural_scroll: bool) {
        if let Some(peer) = self
            .authorized_keys
            .write()
            .expect("lock")
            .get_mut(&fingerprint)
        {
            peer.natural_scroll = natural_scroll;
        }
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn set_incoming_peer_sensitivity(&mut self, fingerprint: String, sensitivity: f64) {
        if let Some(peer) = self
            .authorized_keys
            .write()
            .expect("lock")
            .get_mut(&fingerprint)
        {
            peer.mouse_sensitivity = sensitivity;
        }
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn set_incoming_peer_clipboard_receive(
        &mut self,
        fingerprint: String,
        clipboard_receive: bool,
    ) {
        if let Some(peer) = self
            .authorized_keys
            .write()
            .expect("lock")
            .get_mut(&fingerprint)
        {
            peer.clipboard_receive = clipboard_receive;
        }
        let keys = self.authorized_keys.read().expect("lock").clone();
        // Emulation needs to know so the receive-side gate matches
        // the new value immediately, not after a config-change cycle.
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    async fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
                mode: c.mode,
                network_locks: c.network_locks,
                clipboard_send: c.clipboard_send,
                command_as_ctrl: c.command_as_ctrl,
                require_crossing_modifier: c.require_crossing_modifier,
                crossing_modifier: c.crossing_modifier,
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back().await {
            log::warn!("failed to write config: {e}");
        }
    }

    async fn handle_config_change(&mut self) {
        self.invalidate_file_transfers().await;
        self.pending_clipboards.clear();
        for h in self.client_manager.registered_clients() {
            self.remove_client(h).await;
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let release_threshold = self.config.release_threshold_px();
        self.capture.set_release_threshold(release_threshold);
        self.notify_frontend(FrontendEvent::ReleaseThreshold(release_threshold));
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        self.emulation.set_incoming_peers(authorized_keys);
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    async fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::FileOffer { addr, offer } => {
                let id = offer.offer.transfer_id.0;
                if self.file_offer.as_ref().is_some_and(|current| {
                    current.addr == addr && current.stream.offer.transfer_id.0 == id
                }) || self
                    .active_paste
                    .as_ref()
                    .is_some_and(|current| current.addr == addr && current.id == id)
                {
                    return;
                }
                if offer.offer.origin_fingerprint == self.public_key_fingerprint
                    || offer
                        .offer
                        .validate_against(
                            mousehop_proto::clipboard_transfer::TransferLimits::default(),
                        )
                        .is_err()
                {
                    return;
                }
                // Replace the offer in one frontend event. Clearing the old
                // native marker separately could race preparation of this one.
                self.file_offer.take();
                self.invalidate_file_transfers().await;
                self.clipboard_version = input_capture::clipboard::native_clipboard_version();
                self.native_arming = Some((id, Instant::now()));
                self.file_offer = Some(RemoteFileOffer {
                    addr,
                    stream: offer,
                    clipboard_version: self.clipboard_version,
                });
                self.notify_file_offer();
            }
            EmulationEvent::FileOfferCancelled { addr, id } => {
                if self.file_offer.as_ref().is_some_and(|offer| {
                    offer.addr == addr && offer.stream.offer.transfer_id.0 == id
                }) {
                    self.file_offer = None;
                    self.notify_file_offer();
                }
                if self
                    .active_paste
                    .as_ref()
                    .is_some_and(|paste| paste.addr == addr && paste.id == id)
                {
                    self.cancel_file_paste();
                }
            }
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                if self.pending_pairing.len() < 128 {
                    self.pending_pairing.insert(fingerprint.clone());
                }
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                session,
                pos,
                fingerprint,
            } => {
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, session, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, session, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr, session } => {
                if self
                    .incoming_session(addr)
                    .is_some_and(|current| current != session)
                {
                    log::debug!(
                        "ignoring disconnect for stale inbound session {session} from {addr}"
                    );
                    return;
                }
                if self
                    .file_offer
                    .as_ref()
                    .is_some_and(|offer| offer.addr == addr)
                {
                    self.file_offer = None;
                    self.notify_file_offer();
                }
                if self
                    .active_paste
                    .as_ref()
                    .is_some_and(|paste| paste.addr == addr)
                {
                    self.cancel_file_paste();
                }
                self.mark_remote_host_unavailable(addr);
                if let Some(addr) = self.remove_incoming(addr, session) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    self.discovery.set_port(port);
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify(completion) => {
                self.capture.release_for_handover(completion)
            }
            EmulationEvent::Connected {
                addr,
                session,
                fingerprint,
            } => {
                // Accept itself replaces an inbound transport generation. If
                // the previous generation had already created a return
                // barrier, retire it now rather than waiting for the new peer
                // to cross. The replacement may disconnect before Entered,
                // in which case retaining the old session would make its
                // terminal event look stale forever.
                if let Some(previous_session) = self
                    .incoming_session(addr)
                    .filter(|previous| *previous != session)
                {
                    if self.remove_incoming(addr, previous_session).is_some() {
                        self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                    }
                }
                self.update_incoming_peer_address(addr, &fingerprint).await;
                if let Some(recovery) = self.remote_host_states.get_mut(&fingerprint) {
                    recovery.addr = addr;
                }
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerHello { addr, commit } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.set_peer_commit(handle, Some(commit));
                    self.broadcast_client(handle);
                }
            }
            EmulationEvent::RemoteHostState {
                addr,
                fingerprint,
                state,
            } => {
                let state = match state {
                    mousehop_proto::HostInputState::Locked => RemoteHostState::Locked,
                    mousehop_proto::HostInputState::Unlocked => RemoteHostState::Unlocked,
                };
                let changed = record_remote_host_state(
                    &mut self.remote_host_states,
                    &fingerprint,
                    addr,
                    state,
                );
                if changed {
                    self.notify_frontend(FrontendEvent::RemoteHostState {
                        fingerprint,
                        addr,
                        state,
                    });
                }
            }
            EmulationEvent::ClipboardReceived {
                addr,
                from_fingerprint,
                event,
            } => {
                self.handle_clipboard_received(addr, from_fingerprint, event)
                    .await;
            }
        }
    }

    /// Local clipboard change picked up by the polling
    /// [`ClipboardMonitor`]. Stamp the originator fingerprint on the
    /// wire frame and fan out to every active outgoing client whose
    /// `clipboard_send` is true. Records `(self_fp, hash)` in
    /// `recent_forwarded` so a later forwarded copy of the same
    /// content (re-arriving via another peer in an N-peer ring)
    /// won't be redundantly re-broadcast.
    async fn handle_local_clipboard_event(&mut self, event: Option<input_capture::CaptureEvent>) {
        if !self.sharing {
            return;
        }
        let Some(event) = event else {
            return;
        };
        let input_capture::CaptureEvent::Input(InputEvent::Clipboard(event)) = event else {
            return;
        };
        self.clipboard_version = input_capture::clipboard::native_clipboard_version();
        self.invalidate_file_transfers().await;
        let targets = self.client_manager.clipboard_send_targets();
        if targets.is_empty() {
            log::trace!(
                "{} captured locally but no peer has clipboard_send=true; skipping fan-out",
                clipboard_log_label(&event)
            );
            return;
        }
        let from_fingerprint = self.public_key_fingerprint.clone();
        let hash = clipboard_signature(&event);
        self.prune_recent_forwarded();
        self.recent_forwarded
            .insert((from_fingerprint.clone(), hash), Instant::now());
        log::info!(
            "broadcasting local {} to {} peer(s)",
            clipboard_log_label(&event),
            targets.len()
        );
        self.fan_out_clipboard(event, from_fingerprint, targets)
            .await;
    }

    /// Forwarded clipboard frame just landed via the listen side
    /// (the local clipboard has already been updated by
    /// `emulation::ListenTask`). Refresh the
    /// [`ClipboardMonitor`]'s last-known content so the next 500ms
    /// poll doesn't see this as a fresh local change and bounce it
    /// back, then forward to other peers honoring the recent-
    /// forwarded gate.
    async fn handle_clipboard_received(
        &mut self,
        from_addr: SocketAddr,
        from_fingerprint: String,
        event: ClipboardEvent,
    ) {
        self.invalidate_file_transfers().await;
        if let Some(monitor) = self.clipboard_monitor.as_ref() {
            monitor.update_last_event(&event);
        }
        let hash = clipboard_signature(&event);
        self.prune_recent_forwarded();
        let key = (from_fingerprint.clone(), hash);
        if self.recent_forwarded.contains_key(&key) {
            log::debug!(
                "skipping clipboard re-fan-out: already forwarded ({}, {}) within {}ms",
                &from_fingerprint[..from_fingerprint.len().min(8)],
                clipboard_log_label(&event),
                RECENT_FORWARD_TTL.as_millis()
            );
            return;
        }
        let targets = self.client_manager.clipboard_send_targets();
        let forward_targets: Vec<ClientHandle> = targets
            .into_iter()
            .filter(|h| {
                // Skip the client we just received from. Identified
                // by IP rather than full SocketAddr so a peer's
                // ephemeral source port (which differs between its
                // outgoing and our cached active_addr) doesn't
                // accidentally include them.
                self.client_manager
                    .active_addr(*h)
                    .map(|a| a.ip() != from_addr.ip())
                    .unwrap_or(true)
            })
            .collect();
        if forward_targets.is_empty() {
            return;
        }
        self.recent_forwarded.insert(key, Instant::now());
        log::info!(
            "forwarding {} (originator {}) to {} peer(s)",
            clipboard_log_label(&event),
            &from_fingerprint[..from_fingerprint.len().min(8)],
            forward_targets.len()
        );
        self.fan_out_clipboard(event, from_fingerprint, forward_targets)
            .await;
    }

    async fn fan_out_clipboard(
        &mut self,
        event: ClipboardEvent,
        from_fingerprint: String,
        targets: Vec<ClientHandle>,
    ) {
        let source_version = input_capture::clipboard::native_clipboard_version();
        let is_file = matches!(event, ClipboardEvent::Files { .. });
        let legacy = match &event {
            ClipboardEvent::Text(content) => {
                let encoded_size = 1 + 4 + from_fingerprint.len() + 4 + content.len();
                (encoded_size <= MAX_CLIPBOARD_SIZE).then(|| content.clone())
            }
            ClipboardEvent::ImageRgba { .. } | ClipboardEvent::Files { .. } => None,
        };
        let required_capability = match &event {
            ClipboardEvent::Text(_) => CAP_CLIPBOARD_V2,
            ClipboardEvent::ImageRgba { .. } => CAP_CLIPBOARD_V2 | CAP_CLIPBOARD_IMAGE,
            ClipboardEvent::Files { .. } => {
                CAP_CLIPBOARD_V2 | CAP_CLIPBOARD_FILES | CAP_EXPLICIT_FILE_PASTE
            }
        };
        let needs_bulk = legacy.is_none();
        let mut ready_targets = Vec::new();
        for handle in targets {
            // A newer copy supersedes a deferred copy even if it is small text.
            self.pending_clipboards.remove(&handle);
            if needs_bulk && !self.client_manager.alive(handle) {
                self.pending_clipboards.insert(
                    handle,
                    (event.clone(), from_fingerprint.clone(), Instant::now()),
                );
                // Trigger dialing before interpreting the default zero capabilities.
                let _ = self.conn.send(ProtoEvent::Ping, handle).await;
                continue;
            }
            if needs_bulk
                && self.client_manager.peer_capabilities(handle) & required_capability
                    != required_capability
            {
                log::warn!("client {handle} does not support this clipboard format");
                continue;
            }
            ready_targets.push(handle);
        }
        let targets = ready_targets;
        if targets.is_empty() {
            return;
        }
        if let Some(content) = legacy.as_ref() {
            for handle in targets {
                if let Err(error) = self
                    .conn
                    .send(
                        ProtoEvent::Clipboard {
                            from_fingerprint: from_fingerprint.clone(),
                            content: content.clone(),
                        },
                        handle,
                    )
                    .await
                {
                    log::debug!("clipboard send to client {handle} failed: {error}");
                }
            }
            return;
        }
        // Archive/file-tree preparation can take seconds. Keep the service
        // event loop available for input, Agent heartbeats and cancellation.
        let cancel = self.clipboard_preparations.cancel.clone();
        self.clipboard_preparations.jobs.spawn(async move {
            let epoch = cancel.clone();
            let result = tokio::task::spawn_blocking(move || {
                if let ClipboardEvent::Files { paths, .. } = &event {
                    if !input_capture::clipboard::current_file_copy_matches(paths) {
                        return Err(crate::clipboard_bulk::BulkTransportError::InvalidPayload(
                            "File copy expired".into(),
                        ));
                    }
                }
                #[cfg(windows)]
                let _access = crate::windows_service::desktop_file_access()?;
                match event {
                    ClipboardEvent::Text(content) => OutgoingPayload::text(content),
                    ClipboardEvent::ImageRgba {
                        width,
                        height,
                        bytes,
                    } => OutgoingPayload::image_rgba(width, height, bytes),
                    ClipboardEvent::Files { paths, owned_root } => {
                        OutgoingPayload::file_tree(paths, owned_root, &cancel)
                    }
                }
            })
            .await
            .unwrap_or_else(|error| Err(io::Error::other(error).into()));
            PreparedClipboard {
                cancel: epoch,
                targets,
                origin: from_fingerprint,
                version: source_version,
                is_file,
                capability: required_capability,
                result,
            }
        });
    }

    async fn clipboard_prepared(&mut self, prepared: PreparedClipboard) {
        if !prepared.can_publish(
            self.sharing,
            self.desktop_available,
            input_capture::clipboard::native_clipboard_version(),
        ) {
            return;
        }
        let PreparedClipboard {
            cancel: _,
            mut targets,
            origin: from_fingerprint,
            version: source_version,
            is_file,
            capability,
            result,
        } = prepared;
        if is_file && input_capture::clipboard::native_clipboard_version() != source_version {
            return;
        }
        let payload = match result {
            Ok(payload) => payload,
            Err(error) => {
                log::warn!("failed to prepare clipboard payload: {error}");
                if is_file {
                    let progress = FilePasteProgress {
                        id: rand::random(),
                        received: 0,
                        total: 0,
                        phase: FilePastePhase::Failed,
                        message: Some(format!("Cannot prepare files for transfer: {error}")),
                    };
                    self.last_file_progress = Some(progress.clone());
                    self.notify_frontend(FrontendEvent::FilePasteProgress(progress));
                }
                return;
            }
        };
        let enabled = self.client_manager.clipboard_send_targets();
        targets.retain(|handle| {
            enabled.contains(handle)
                && self.client_manager.alive(*handle)
                && self.client_manager.peer_capabilities(*handle) & capability == capability
        });
        for handle in targets {
            let Some(addr) = self.client_manager.active_addr(handle) else {
                log::debug!("clipboard-v2 client {handle} has no active address");
                continue;
            };
            let transfer =
                match OutgoingTransfer::bind(addr.ip(), from_fingerprint.clone(), payload.clone())
                    .await
                {
                    Ok(transfer) => transfer,
                    Err(error) => {
                        log::warn!("failed to bind clipboard-v2 transfer for {handle}: {error}");
                        continue;
                    }
                };
            let transfer_id = transfer.offer().offer.transfer_id.0;
            if let Err(error) = self
                .conn
                .send(ProtoEvent::ClipboardStreamOffer(transfer.offer()), handle)
                .await
            {
                log::debug!("clipboard-v2 offer to client {handle} failed: {error}");
                continue;
            }
            let task = tokio::task::spawn_local(async move {
                if let Err(error) = transfer
                    .serve_if_current(if is_file { source_version } else { None })
                    .await
                {
                    log::warn!("clipboard-v2 transfer to client {handle} failed: {error}");
                }
            });
            if is_file {
                self.file_senders.insert(
                    handle,
                    OutgoingFile {
                        id: transfer_id,
                        task,
                    },
                );
            }
        }
    }

    fn fail_native_paste(&mut self, id: [u8; 16], message: String) {
        if self
            .file_offer
            .as_ref()
            .is_none_or(|offer| offer.stream.offer.transfer_id.0 != id)
        {
            return;
        }
        self.file_offer = None;
        self.native_arming = None;
        self.notify_file_offer();
        let progress = FilePasteProgress {
            id,
            received: 0,
            total: 0,
            phase: FilePastePhase::Failed,
            message: Some(message),
        };
        self.last_file_progress = Some(progress.clone());
        self.notify_frontend(FrontendEvent::FilePasteProgress(progress));
    }

    fn notify_file_offer(&mut self) {
        let summary = self.file_offer.as_ref().map(|offer| {
            let mut summary = offer.summary();
            summary.source = self
                .authorized_keys
                .read()
                .unwrap()
                .get(&summary.source)
                .map(|peer| peer.description.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| offer.addr.ip().to_string());
            summary
        });
        self.notify_frontend(FrontendEvent::FilePasteAvailable(summary));
    }

    fn cancel_file_paste(&mut self) {
        if let Some(paste) = self.active_paste.as_ref() {
            // Keep this operation active until its blocking publisher has
            // restored backups; a new paste must not race that rollback.
            paste.cancel.cancel();
        }
    }

    async fn invalidate_file_transfers(&mut self) {
        self.clipboard_preparations.cancel();
        self.native_arming = None;
        // Closing the source listener is authoritative even if the revocation
        // datagram is lost: an obsolete offer cannot fetch file contents.
        let senders = std::mem::take(&mut self.file_senders);
        for (handle, sender) in senders {
            let id = sender.id;
            drop(sender);
            let _ = self
                .conn
                .send(ProtoEvent::FilePasteCancel(id), handle)
                .await;
        }
        self.pending_clipboards
            .retain(|_, (event, _, _)| !matches!(event, ClipboardEvent::Files { .. }));
        if self.file_offer.take().is_some() {
            self.notify_file_offer();
        }
        self.cancel_file_paste();
    }

    fn start_file_paste(&mut self, id: [u8; 16], destination: std::path::PathBuf) {
        if !self.sharing || !self.desktop_available || self.desktop_agent_seen.is_none() {
            return;
        }
        if self.capture.is_forwarding() {
            self.fail_native_paste(
                id,
                "Input is directed to another computer; paste on the receiving computer".into(),
            );
            return;
        }
        if self.active_paste.is_some() {
            self.notify_frontend(FrontendEvent::Error(
                "A file paste is already in progress".into(),
            ));
            return;
        }
        let valid = self.file_offer.as_ref().is_some_and(|offer| {
            offer.is_current(id, input_capture::clipboard::native_clipboard_version())
        });
        if !valid {
            self.notify_frontend(FrontendEvent::Error(
                "This file copy expired; copy the files again".into(),
            ));
            return;
        }
        let offer = self.file_offer.take().expect("validated offer");
        let addr = offer.addr;
        let total = offer.stream.offer.total_bytes;
        self.native_arming = None;
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::task::spawn_local(crate::file_paste::paste(
            offer,
            destination,
            cancel.clone(),
            self.file_progress_tx.clone(),
            self.file_conflict_tx.clone(),
        ));
        self.active_paste = Some(ActivePaste {
            addr,
            id,
            cancel,
            task,
            conflict: None,
        });
        // The GUI must know the paste is active before the pending offer is
        // cleared, so its native marker survives until transfer completion.
        let initial = FilePasteProgress {
            id,
            received: 0,
            total,
            phase: FilePastePhase::Receiving,
            message: None,
        };
        self.file_paste_progress(initial);
        self.notify_file_offer();
    }

    fn file_paste_conflict(&mut self, conflict: PendingFileConflict) {
        let Some(active) = self
            .active_paste
            .as_mut()
            .filter(|paste| paste.id == conflict.question.id)
        else {
            return;
        };
        let question = conflict.question.clone();
        active.conflict = Some(conflict);
        let (received, total) = self
            .last_file_progress
            .as_ref()
            .filter(|progress| progress.id == question.id)
            .map(|progress| (progress.received, progress.total))
            .unwrap_or((0, 0));
        self.file_paste_progress(FilePasteProgress {
            id: question.id,
            received,
            total,
            phase: FilePastePhase::WaitingForConflict,
            message: None,
        });
        self.notify_frontend(FrontendEvent::FilePasteConflict(question));
    }

    fn file_paste_progress(&mut self, progress: FilePasteProgress) {
        if self
            .active_paste
            .as_ref()
            .is_none_or(|paste| paste.id != progress.id)
        {
            return;
        }
        let finished = matches!(
            progress.phase,
            FilePastePhase::Complete | FilePastePhase::Failed | FilePastePhase::Cancelled
        );
        if finished {
            self.active_paste.take();
        }
        self.last_file_progress = Some(progress.clone());
        self.notify_frontend(FrontendEvent::FilePasteProgress(progress));
        if finished {
            self.notify_file_offer();
        }
    }

    async fn retry_pending_clipboards(&mut self) {
        let enabled: HashSet<_> = self
            .client_manager
            .clipboard_send_targets()
            .into_iter()
            .collect();
        let ready = take_ready_clipboards(&mut self.pending_clipboards, &enabled, |handle| {
            self.client_manager.alive(handle)
        });
        for (handle, (event, origin, _)) in ready {
            self.fan_out_clipboard(event, origin, vec![handle]).await;
        }
        for &handle in self.pending_clipboards.keys() {
            let _ = self.conn.send(ProtoEvent::Ping, handle).await;
        }
    }

    fn prune_recent_forwarded(&mut self) {
        self.recent_forwarded
            .retain(|_, ts| ts.elapsed() < RECENT_FORWARD_TTL);
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::InputForwarding(forwarding) => {
                self.notify_frontend(FrontendEvent::InputForwarding(forwarding))
            }
            ICaptureEvent::CaptureBegin { handle, handover } => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation
                        .send_leave_event(incoming.addr, incoming.session, handover);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                self.spawn_hook_command(handle);
            }
            ICaptureEvent::PeerCommitUpdated(handle) => {
                self.broadcast_client(handle);
            }
        }
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    /// Apply a background latency sample and re-broadcast the client
    /// only when the stored value actually moved. The reading is
    /// quantized to 100 µs buckets so ordinary sub-microsecond jitter
    /// on a stable link doesn't churn the frontend every probe cycle.
    /// Also feeds the `Fastest`-mode convergence check.
    async fn handle_probe_result(&mut self, result: ProbeResult) {
        let ProbeResult {
            handle,
            ip,
            rtt_micros,
        } = result;
        let bucketed = rtt_micros.map(|us| us - (us % 100));
        if self.client_manager.set_latency(handle, ip, bucketed) {
            self.broadcast_client(handle);
        }
        self.maybe_upgrade_fastest(handle).await;
    }

    /// In `Fastest` mode (and only when not locked), nudge the live
    /// connection toward the lowest-latency candidate — but only once a
    /// candidate has been *sustainedly and substantially* faster than
    /// the active path (see [`ClientManager::fastest_upgrade_candidate`]
    /// and [`FASTEST_UPGRADE_STREAK`]), so probe noise can't cause
    /// flapping. Tearing down the connection makes the next send
    /// re-dial, which picks the new fastest.
    async fn maybe_upgrade_fastest(&mut self, handle: ClientHandle) {
        let eligible = self.client_manager.get_active_lock(handle).is_none()
            && self.client_manager.get_mode(handle) == ConnectionMode::Fastest;
        if !eligible {
            self.fastest_streak.remove(&handle);
            return;
        }
        if self
            .client_manager
            .fastest_upgrade_candidate(handle)
            .is_some()
        {
            let streak = self.fastest_streak.entry(handle).or_insert(0);
            *streak += 1;
            if *streak >= FASTEST_UPGRADE_STREAK {
                self.fastest_streak.remove(&handle);
                log::info!("client {handle}: switching to a sustainedly-faster interface");
                self.conn.reset_handle(handle).await;
            }
        } else {
            self.fastest_streak.remove(&handle);
        }
    }

    /// Fold a resolved peer advertisement into every client whose
    /// configured hostname matches the mDNS instance: add the
    /// discovered addresses to the candidate set and record per-address
    /// interface kinds. Re-broadcasts only the clients that changed.
    fn handle_discovery_event(&mut self, event: DiscoveryEvent) {
        let DiscoveryEvent::Resolved {
            instance,
            addresses,
            interfaces,
        } = event;
        for handle in self.client_manager.registered_clients() {
            let matches = self
                .client_manager
                .get_hostname(handle)
                .is_some_and(|h| crate::discovery::normalize_mdns_name(&h) == instance);
            if !matches {
                continue;
            }
            // Discovered ips must land before interface labels so the
            // label filter (which keeps only current candidates) sees
            // the freshly-added addresses.
            let discovered_changed = self
                .client_manager
                .set_discovered_ips(handle, addresses.clone());
            let interfaces_changed = self
                .client_manager
                .set_interfaces(handle, interfaces.clone());
            if discovered_changed {
                // The candidate set moved, so any in-progress
                // Fastest-upgrade streak no longer describes the
                // current candidates — start it fresh.
                self.fastest_streak.remove(&handle);
            }
            if discovered_changed || interfaces_changed {
                self.broadcast_client(handle);
            }
        }
    }

    /// Periodic discovery housekeeping: re-publish our advertisement
    /// (primary interface may have changed) and re-evaluate which
    /// network we're on for per-network lock resolution.
    async fn on_discovery_refresh(&mut self) {
        self.discovery.refresh();
        self.refresh_network().await;
    }

    /// Recompute the current-network fingerprint. On change, re-resolve
    /// every client's active lock and re-dial those whose lock moved so
    /// the new network's pin (or its absence) takes effect at once.
    async fn refresh_network(&mut self) {
        let net = current_network_id();
        if net == self.current_network {
            return;
        }
        log::info!("network changed: {:?} -> {:?}", self.current_network, net);
        self.current_network = net;
        for handle in self.client_manager.registered_clients() {
            if self
                .client_manager
                .recompute_active_lock(handle, self.current_network.as_deref())
            {
                self.conn.reset_handle(handle).await;
                self.broadcast_client(handle);
            }
        }
    }

    fn sync_frontend(&mut self) {
        for fingerprint in self.pending_pairing.iter().cloned().collect::<Vec<_>>() {
            self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
        }
        self.notify_frontend(FrontendEvent::ConfigureAutostartStatus);
        self.notify_frontend(FrontendEvent::DesktopAvailable(self.desktop_available));
        self.notify_frontend(FrontendEvent::ProtocolVersion(
            mousehop_ipc::LOCAL_PROTOCOL_VERSION,
        ));
        self.notify_frontend(FrontendEvent::RuntimeState(if self.stopped {
            mousehop_ipc::RuntimeState::Stopped
        } else if self.sharing {
            mousehop_ipc::RuntimeState::Running
        } else {
            mousehop_ipc::RuntimeState::Paused
        }));
        self.notify_frontend(FrontendEvent::InputForwarding(self.capture.is_forwarding()));
        self.notify_file_offer();
        if self.file_offer.is_none() {
            if let Some(progress) = self.last_file_progress.clone() {
                self.notify_frontend(FrontendEvent::FilePasteProgress(progress));
            }
        }
        if let Some(question) = self
            .active_paste
            .as_ref()
            .and_then(|paste| paste.conflict.as_ref())
            .map(|pending| pending.question.clone())
        {
            self.notify_frontend(FrontendEvent::FilePasteConflict(question));
        }
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        self.notify_frontend(FrontendEvent::ReleaseThreshold(
            self.config.release_threshold_px(),
        ));
        self.notify_frontend(FrontendEvent::ReleaseBind(self.config.release_bind()));
        self.notify_frontend(FrontendEvent::MdnsDiscovery(self.config.mdns_discovery()));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        let host_list = self.config.clipboard_suppression().host().clone();
        self.notify_frontend(FrontendEvent::SuppressedAppsUpdated(host_list));
        let recovery_events: Vec<_> = self
            .remote_host_states
            .iter()
            .map(|(fingerprint, recovery)| FrontendEvent::RemoteHostState {
                fingerprint: fingerprint.clone(),
                addr: recovery.addr,
                state: recovery.state,
            })
            .collect();
        for event in recovery_events {
            self.notify_frontend(event);
        }
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(
        &mut self,
        addr: SocketAddr,
        session: ListenerSession,
        pos: Position,
        fingerprint: String,
    ) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture
            .create(handle, pos, CaptureType::EnterOnly, false, None);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                session,
                pos,
            },
        );
    }

    fn update_incoming(
        &mut self,
        addr: SocketAddr,
        session: ListenerSession,
        pos: Position,
        fingerprint: String,
    ) {
        let incoming = self
            .incoming_conn_info
            .values()
            .find(|incoming| incoming.addr == addr)
            .expect("no such client");
        let previous_session = incoming.session;
        let changed = incoming.session != session
            || incoming.fingerprint != fingerprint
            || incoming.pos != pos;
        if changed {
            self.remove_incoming(addr, previous_session);
            self.add_incoming(addr, session, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn incoming_session(&self, addr: SocketAddr) -> Option<ListenerSession> {
        self.incoming_conn_info
            .values()
            .find(|incoming| incoming.addr == addr)
            .map(|incoming| incoming.session)
    }

    fn remove_incoming(
        &mut self,
        addr: SocketAddr,
        session: ListenerSession,
    ) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr && incoming.session == session)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn mark_remote_host_unavailable(&mut self, addr: SocketAddr) {
        let update = mark_locked_remote_host_unavailable(&mut self.remote_host_states, addr);
        if let Some((fingerprint, addr)) = update {
            self.notify_frontend(FrontendEvent::RemoteHostState {
                fingerprint,
                addr,
                state: RemoteHostState::Unavailable,
            });
        }
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        log::info!("Authorizing incoming peer certificate {fp}");
        // New authorizations land with default post-processing; the
        // user can tune natural-scroll / sensitivity from the
        // expanded row in the Incoming Connections list.
        let entry = IncomingPeerConfig {
            description: desc,
            ..IncomingPeerConfig::default()
        };
        self.authorized_keys
            .write()
            .expect("lock")
            .insert(fp, entry);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.emulation.set_incoming_peers(keys.clone());
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            let command_as_ctrl = self.client_manager.command_as_ctrl(handle);
            let crossing_modifier = self.client_manager.required_crossing_modifier(handle);
            self.capture.create(
                handle,
                pos,
                CaptureType::Default,
                command_as_ctrl,
                crossing_modifier,
            );
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    async fn remove_client(&mut self, handle: ClientHandle) {
        self.file_senders.remove(&handle);
        self.pending_clipboards.remove(&handle);
        // Retire the exact outbound DTLS generation before Slab can reuse this
        // numeric handle for a different configured peer. The remote side
        // releases its emulation handle on connection close, and Capture gets
        // the matching terminal event before any replacement can dial.
        self.conn.reset_handle(handle).await;
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    fn spawn_hook_command(&self, handle: ClientHandle) {
        #[cfg(windows)]
        if crate::windows_service::worker_mode() || mousehop_ipc::desktop::current().is_some() {
            return; // Never execute user-configured shell hooks as LocalSystem.
        }
        let Some(cmd) = self.client_manager.get_enter_cmd(handle) else {
            return;
        };
        tokio::task::spawn_local(async move {
            log::info!("spawning command!");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute cmd: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{cmd} exited successfully");
                    } else {
                        log::warn!("{cmd} exited with {s}");
                    }
                }
                Err(e) => log::warn!("{cmd}: {e}"),
            }
        });
    }
}

/// Missing or completed permission watchers must not busy-spin Core's loop.
async fn wait_for_permission_revocation(
    receiver: &mut Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
) {
    if let Some(receiver) = receiver.as_mut() {
        if receiver.recv().await.is_some() {
            return;
        }
    }
    *receiver = None;
    std::future::pending::<()>().await;
}

/// Wait for clipboard changes without busy-spinning when monitoring is absent.
async fn recv_clipboard(
    monitor: &mut Option<ClipboardMonitor>,
) -> Option<input_capture::CaptureEvent> {
    match monitor.as_mut() {
        Some(m) => m.recv().await,
        None => std::future::pending().await,
    }
}

/// `tokio::select!` arm helper for the discovery event stream. Pends
/// forever once the channel is closed so the `select!` doesn't
/// busy-spin during shutdown.
async fn recv_discovery(rx: &mut Receiver<DiscoveryEvent>) -> DiscoveryEvent {
    match rx.recv().await {
        Some(event) => event,
        None => std::future::pending().await,
    }
}

#[cfg(windows)]
async fn recv_desktop_clipboard(
    rx: &mut Option<tokio::sync::mpsc::Receiver<(u64, ClipboardEvent)>>,
) -> ClipboardEvent {
    if let Some(rx) = rx {
        while let Some((generation, event)) = rx.recv().await {
            if mousehop_ipc::desktop::current()
                .is_some_and(|link| link.generation() != generation || link.secure())
            {
                continue;
            }
            return event;
        }
    }
    std::future::pending().await
}
#[cfg(not(windows))]
async fn recv_desktop_clipboard(
    _rx: &mut Option<tokio::sync::mpsc::Receiver<(u64, ClipboardEvent)>>,
) -> ClipboardEvent {
    std::future::pending().await
}

#[cfg(test)]
mod tests {
    #[test]
    fn completed_preparation_cannot_publish_after_pause_desktop_change_or_new_copy() {
        let preparations = super::ClipboardPreparations::default();
        let prepared = super::PreparedClipboard {
            cancel: preparations.cancel.clone(),
            targets: vec![],
            origin: String::new(),
            version: Some(17),
            is_file: true,
            capability: 0,
            result: super::OutgoingPayload::text("archive ready".into()),
        };
        assert!(prepared.can_publish(true, true, Some(17)));
        assert!(!prepared.can_publish(false, true, Some(17)));
        assert!(!prepared.can_publish(true, false, Some(17)));
        assert!(!prepared.can_publish(true, true, Some(18)));
        drop(preparations);
        assert!(!prepared.can_publish(true, true, Some(17)));
    }

    #[tokio::test]
    async fn newer_copy_cancels_pending_preparation_and_shutdown_cancels_archiving() {
        let mut preparations = super::ClipboardPreparations::default();
        let previous = preparations.cancel.clone();
        preparations
            .jobs
            .spawn(std::future::pending::<super::PreparedClipboard>());
        preparations.cancel();
        assert!(previous.is_cancelled());
        assert!(!preparations.cancel.is_cancelled());
        assert!(
            matches!(preparations.jobs.join_next().await, Some(Err(error)) if error.is_cancelled())
        );
        let current = preparations.cancel.clone();
        drop(preparations);
        assert!(current.is_cancelled());
    }

    use super::*;

    #[test]
    fn deferred_clipboard_waits_for_connection_and_honors_revocation_and_timeout() {
        let item = || {
            (
                ClipboardEvent::Text("bulk copy".into()),
                "origin".into(),
                Instant::now(),
            )
        };
        let mut pending = HashMap::from([(0, item()), (1, item()), (2, item())]);
        pending.get_mut(&2).unwrap().2 = Instant::now() - Duration::from_secs(16);
        let enabled = HashSet::from([0, 2]);
        assert!(take_ready_clipboards(&mut pending, &enabled, |_| false).is_empty());
        assert_eq!(pending.len(), 1);
        let ready = take_ready_clipboards(&mut pending, &enabled, |_| true);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, 0);
        assert!(pending.is_empty());
    }

    #[test]
    fn clipboard_hash_is_deterministic_within_run() {
        let h1 = clipboard_hash("hello, world");
        let h2 = clipboard_hash("hello, world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn clipboard_hash_distinguishes_different_inputs() {
        assert_ne!(clipboard_hash("foo"), clipboard_hash("bar"));
        assert_ne!(clipboard_hash(""), clipboard_hash("\0"));
    }

    #[test]
    fn clipboard_transport_preserves_legacy_and_gates_bulk() {
        assert_eq!(
            clipboard_transport(MAX_CLIPBOARD_SIZE, 0),
            ClipboardTransport::Legacy
        );
        assert_eq!(
            clipboard_transport(MAX_CLIPBOARD_SIZE + 1, 0),
            ClipboardTransport::Unsupported
        );
        assert_eq!(
            clipboard_transport(MAX_CLIPBOARD_SIZE + 1, CAP_CLIPBOARD_V2),
            ClipboardTransport::Bulk
        );
    }

    #[test]
    fn recent_forwarded_prune_evicts_expired_entries() {
        // Mirrors `Service::prune_recent_forwarded` so the eviction
        // contract is documented as code rather than implicit in
        // `HashMap::retain`.
        let mut map: HashMap<(String, u64), Instant> = HashMap::new();
        let now = Instant::now();
        let stale = now
            .checked_sub(Duration::from_secs(2))
            .expect("clock far enough from epoch for the test to subtract 2s");
        map.insert(("fp_a".into(), 1), stale);
        map.insert(("fp_b".into(), 2), now);
        map.retain(|_, ts| ts.elapsed() < RECENT_FORWARD_TTL);
        assert!(!map.contains_key(&("fp_a".to_string(), 1)));
        assert!(map.contains_key(&("fp_b".to_string(), 2)));
    }

    #[test]
    fn disconnect_downgrades_confirmed_lock_to_unavailable() {
        let addr: SocketAddr = "192.0.2.4:4252".parse().expect("address");
        let mut states = HashMap::from([(
            "peer-fingerprint".to_owned(),
            RemoteHostRecovery {
                addr,
                state: RemoteHostState::Locked,
            },
        )]);

        assert_eq!(
            mark_locked_remote_host_unavailable(&mut states, addr),
            Some(("peer-fingerprint".to_owned(), addr))
        );
        assert_eq!(
            states["peer-fingerprint"].state,
            RemoteHostState::Unavailable
        );
        assert_eq!(
            mark_locked_remote_host_unavailable(&mut states, addr),
            None,
            "an unavailable peer must not be reported as a fresh lock"
        );
    }

    #[test]
    fn repeated_wire_state_does_not_reopen_recovery_ui() {
        let addr: SocketAddr = "192.0.2.4:4252".parse().expect("address");
        let mut states = HashMap::new();
        assert!(record_remote_host_state(
            &mut states,
            "peer-fingerprint",
            addr,
            RemoteHostState::Locked,
        ));
        assert!(!record_remote_host_state(
            &mut states,
            "peer-fingerprint",
            addr,
            RemoteHostState::Locked,
        ));
        assert!(record_remote_host_state(
            &mut states,
            "peer-fingerprint",
            addr,
            RemoteHostState::Unlocked,
        ));
        assert!(!record_remote_host_state(
            &mut states,
            "peer-fingerprint",
            addr,
            RemoteHostState::Unlocked,
        ));
    }
}
