//! `cdct serve`: the one writer, its listeners and its pushed events.
//!
//! The daemon owns the holder table behind the existing exclusive lock, takes
//! the decisions from [`Conductor`] and never executes anything. It listens in
//! two places with the same JSON-lines protocol:
//!
//! - the host's local Unix socket, restricted to a group, optionally inherited
//!   from systemd socket activation (`LISTEN_FDS`);
//! - one Unix socket per configured guest channel. The hypervisor connects its
//!   virtio-serial port to that path, so the listener itself identifies the
//!   instance and no credential has to travel through the guest.
//!
//! Every state change is written to the table and pushed to subscribers. The
//! daemon has no timer and polls nothing.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use cdrv::{
    Event, EventLog, GpuLadder, HolderId, InstanceClass, InstanceId, InstanceState, KernelOwner,
    LeaseId, Source, Timestamp,
};
use listenfd::ListenFd;

use crate::config::{Config, InstanceConfig};
use crate::engine::{Conductor, FactsFile};
use crate::facts::MemorySnapshot;
use crate::gpu::{GpuState, GpuTenant, InstanceObservation, TenantState};
use crate::holders::{Holder, HolderState, HolderStore, HolderTable};
use crate::plan::SwitchOutcome;
use crate::protocol::{
    Availability, Channel, HolderSummary, Reply, Request, Status, TargetStatus, decode, encode,
};
use crate::reader::ProcReader;

/// The largest accepted request line.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// A bound daemon, kept alive for the life of the process.
///
/// The accept threads stop when this value is dropped: it marks them stopped
/// and connects once to each socket to wake the blocking accept.
pub struct Running {
    local: PathBuf,
    guest: Vec<(InstanceId, PathBuf)>,
    bound: Vec<PathBuf>,
    activated: bool,
    stopped: Arc<AtomicBool>,
    /// Holds the state (and the writer lock) for the life of the daemon.
    _shared: Arc<Mutex<Inner>>,
}

impl Running {
    /// The configured local control socket.
    #[must_use]
    pub fn local_socket(&self) -> &Path {
        &self.local
    }

    /// The path of a guest's channel listener, when configured.
    #[must_use]
    pub fn guest_socket(&self, instance: &InstanceId) -> Option<&Path> {
        self.guest
            .iter()
            .find(|(id, _)| id == instance)
            .map(|(_, path)| path.as_path())
    }

    /// Every configured guest channel and its path.
    #[must_use]
    pub fn guest_sockets(&self) -> &[(InstanceId, PathBuf)] {
        &self.guest
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        // Wake each blocking accept once; the loop sees the flag and stops.
        let _ = UnixStream::connect(&self.local);
        for path in &self.bound {
            let _ = UnixStream::connect(path);
        }
        if !self.activated {
            let _ = fs::remove_file(&self.local);
        }
        for (_, path) in &self.guest {
            let _ = fs::remove_file(path);
        }
    }
}

/// Everything the daemon keeps under its one lock.
struct Inner {
    config: Config,
    conductor: Conductor,
    table: HolderTable,
    store: HolderStore,
    log: EventLog,
    subscribers: Vec<Sender<String>>,
}

impl Inner {
    /// The memory and GPU facts one decision sees.
    fn facts(&self) -> Result<(MemorySnapshot, GpuState), String> {
        let document = match &self.config.facts {
            Some(path) => {
                let facts = FactsFile::read(path).map_err(|error| error.to_string())?;
                Some((path.clone(), facts))
            }
            None => None,
        };
        let memory = match &document {
            Some((path, facts)) => facts
                .memory_snapshot(path)
                .map_err(|error| error.to_string())?,
            None => {
                let root = &self.config.proc_root;
                let raw = ProcReader::new(root)
                    .raw_facts()
                    .map_err(|error| format!("cannot read {}: {error}", root.display()))?;
                MemorySnapshot::parse(&raw).map_err(|error| error.to_string())?
            }
        };
        let declared = document.is_none();
        let mut gpu = document.map(|(_, facts)| facts.gpu).unwrap_or_default();
        if declared {
            // No facts document: the declared tenants hold the card exactly
            // while the host kernel does.
            let state = if self.table.gpu_owner.is_host() {
                TenantState::Running
            } else {
                TenantState::Stopped
            };
            gpu.tenants = self
                .config
                .tenants
                .iter()
                .map(|tenant| GpuTenant {
                    id: tenant.id.clone(),
                    ladder: tenant.ladder,
                    state,
                })
                .collect();
        }
        if !self.config.gpu_device.is_empty() {
            gpu.device = self.config.gpu_device.clone();
        }
        gpu.owner = self.table.gpu_owner.clone();
        gpu.bound_to_vfio = !gpu.owner.is_host();
        self.merge_instances(&mut gpu);
        Ok((memory, gpu))
    }

    /// Add the configured instances a facts document did not observe, and mark
    /// every instance with a holder (and the card's owner) live.
    fn merge_instances(&self, gpu: &mut GpuState) {
        for configured in &self.config.instances {
            if !gpu
                .instances
                .iter()
                .any(|observed| observed.id == configured.id)
            {
                gpu.instances.push(self.observation(configured));
            }
        }
        for holder in &self.table.holders {
            if let Some(instance) = &holder.instance
                && let Some(observed) = gpu
                    .instances
                    .iter_mut()
                    .find(|observed| &observed.id == instance)
            {
                observed.state = InstanceState::Running;
                observed.resident_mib = Some(holder.memory_mib);
            }
        }
        if let Some(instance) = gpu.owner.guest()
            && let Some(observed) = gpu
                .instances
                .iter_mut()
                .find(|observed| &observed.id == instance)
        {
            observed.state = InstanceState::Running;
        }
    }

    fn observation(&self, configured: &InstanceConfig) -> InstanceObservation {
        let holder = self
            .table
            .holders
            .iter()
            .find(|holder| holder.instance.as_ref() == Some(&configured.id));
        InstanceObservation {
            id: configured.id.clone(),
            class: configured.class,
            guest_os: configured.guest_os,
            state: if holder.is_some() {
                InstanceState::Running
            } else {
                InstanceState::Stopped
            },
            memory_mib: configured
                .memory_mib
                .or_else(|| holder.map(|holder| holder.memory_mib)),
            resident_mib: holder
                .map(|holder| holder.memory_mib)
                .or(configured.resident_mib),
            install: configured.install.clone(),
        }
    }

    fn status(&self) -> Result<Status, String> {
        let (memory, gpu) = self.facts()?;
        let targets = self
            .config
            .targets
            .iter()
            .map(|target| {
                let outcome = self
                    .conductor
                    .switch(&target.kernel, &gpu, &memory, &self.table);
                TargetStatus {
                    name: target.name.clone(),
                    label: target.label.clone(),
                    kernel: target.kernel.clone(),
                    availability: Availability::from_outcome(outcome),
                }
            })
            .collect();
        Ok(Status {
            owner: self.table.gpu_owner.clone(),
            owner_label: self.config.owner_label(&self.table.gpu_owner),
            seq: self.table.seq,
            holders: self
                .table
                .holders
                .iter()
                .map(|holder| HolderSummary {
                    id: holder.id.to_string(),
                    instance: holder.instance.clone(),
                    tenant: holder.tenant.clone(),
                    class: holder.class,
                    priority: holder.priority,
                    memory_mib: holder.memory_mib,
                    devices: holder.devices.clone(),
                    state: holder.state,
                })
                .collect(),
            targets,
        })
    }

    fn plan(&self, target: &KernelOwner) -> Result<SwitchOutcome, String> {
        let (memory, gpu) = self.facts()?;
        Ok(self.conductor.switch(target, &gpu, &memory, &self.table))
    }

    /// Decide a switch and, for a plan, record its grant. Returns the outcome
    /// and the events the change emitted (empty for a wait or a decline).
    fn switch(&mut self, target: &KernelOwner) -> Result<(SwitchOutcome, Vec<Event>), String> {
        let outcome = self.plan(target)?;
        let mut events = Vec::new();
        if let SwitchOutcome::Plan(plan) = &outcome {
            let at = now();
            let custodians: Vec<HolderId> = self
                .table
                .holders
                .iter()
                .filter(|holder| holder.is_card_custodian())
                .map(|holder| holder.id.clone())
                .collect();
            match target {
                KernelOwner::Host => {
                    for id in custodians {
                        if let Some(event) = self.table.release(
                            &id,
                            &mut self.log,
                            at,
                            "switched to the host kernel",
                        ) {
                            events.push(event);
                        }
                    }
                }
                KernelOwner::Guest(instance) => {
                    for id in custodians {
                        if let Some(event) = self.table.release(
                            &id,
                            &mut self.log,
                            at,
                            "the card moved to another kernel",
                        ) {
                            events.push(event);
                        }
                    }
                    let class = self
                        .config
                        .instance(instance)
                        .map_or(InstanceClass::Desktop, |configured| configured.class);
                    let memory_mib = plan
                        .memory
                        .as_ref()
                        .map_or(0, |admission| admission.required_mib);
                    let holder = Holder {
                        id: HolderId(format!("card-{instance}")),
                        lease: LeaseId(format!("lease-{instance}-{}", self.log.next_seq())),
                        instance: Some(instance.clone()),
                        tenant: None,
                        class,
                        priority: GpuLadder::Desktop.priority(),
                        memory_mib,
                        cpu_cores: 0.0,
                        devices: vec![self.config.gpu_device.clone()],
                        displaced: plan.displaced.clone(),
                        state: HolderState::Granted,
                        since: at,
                        last_event_seq: 0,
                    };
                    events.push(self.table.admit(holder, &mut self.log, at));
                }
            }
            self.store
                .save(&self.table)
                .map_err(|error| error.to_string())?;
        }
        Ok((outcome, events))
    }

    /// Push the events of one change, then a fresh status snapshot.
    fn publish(&mut self, events: &[Event]) {
        let status = self.status().ok();
        for event in events {
            self.broadcast(&Reply::Event {
                event: event.clone(),
            });
        }
        if let Some(status) = status {
            self.broadcast(&Reply::Status { status });
        }
    }

    fn broadcast(&mut self, reply: &Reply) {
        let Ok(line) = encode(reply) else {
            return;
        };
        self.subscribers
            .retain(|subscriber| subscriber.send(line.clone()).is_ok());
    }
}

/// Start the daemon: take the writer lock, load the table, bind the listeners
/// and serve until the process ends.
pub fn start(config: Config) -> Result<Running, ServeError> {
    let mut store = HolderStore::try_open(&config.state)?;
    let table = store.load()?;
    let log = EventLog::new(
        config.host.clone(),
        Source::Cdct,
        table.seq.saturating_add(1),
    );
    let conductor = Conductor::new(config.policy.clone());
    let shared = Arc::new(Mutex::new(Inner {
        config: config.clone(),
        conductor,
        table,
        store,
        log,
        subscribers: Vec::new(),
    }));
    let stopped = Arc::new(AtomicBool::new(false));
    let mut bound = Vec::new();
    let mut guest = Vec::new();

    let (local_listener, activated) = match activated_listener()? {
        Some(listener) => (listener, true),
        None => {
            let listener = bind_listener(&config.socket, &config, &mut bound)?;
            (listener, false)
        }
    };
    spawn_accept(
        local_listener,
        Arc::clone(&shared),
        Channel::Local,
        Arc::clone(&stopped),
    );

    for instance in &config.instances {
        let Some(path) = &instance.channel else {
            continue;
        };
        let listener = bind_listener(path, &config, &mut bound)?;
        guest.push((instance.id.clone(), path.clone()));
        spawn_accept(
            listener,
            Arc::clone(&shared),
            Channel::Guest {
                instance: instance.id.clone(),
            },
            Arc::clone(&stopped),
        );
    }

    Ok(Running {
        local: config.socket,
        guest,
        bound,
        activated,
        stopped,
        _shared: shared,
    })
}

/// Take the local listener systemd passed in `LISTEN_FDS`, when there is one.
fn activated_listener() -> Result<Option<UnixListener>, ServeError> {
    let mut listenfd = ListenFd::from_env();
    if listenfd.len() == 0 {
        return Ok(None);
    }
    listenfd
        .take_unix_listener(0)
        .map_err(ServeError::Activation)
}

fn bind_listener(
    path: &Path,
    config: &Config,
    bound: &mut Vec<PathBuf>,
) -> Result<UnixListener, ServeError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| ServeError::Socket {
            path: path.display().to_string(),
            source,
        })?;
    }
    // The writer lock is held, so a socket file left here is stale (the
    // previous daemon was killed before it could remove it).
    if fs::symlink_metadata(path)
        .is_ok_and(|m| std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()))
    {
        let _ = fs::remove_file(path);
    }
    let listener = UnixListener::bind(path).map_err(|source| ServeError::Socket {
        path: path.display().to_string(),
        source,
    })?;
    restrict(path, config.socket_mode, config.socket_gid)?;
    bound.push(path.to_path_buf());
    Ok(listener)
}

/// Restrict a socket to its group and mode.
fn restrict(path: &Path, mode: u32, gid: Option<u32>) -> Result<(), ServeError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
        ServeError::Socket {
            path: path.display().to_string(),
            source,
        }
    })?;
    if let Some(gid) = gid {
        std::os::unix::fs::chown(path, None, Some(gid)).map_err(|source| ServeError::Socket {
            path: path.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

fn spawn_accept(
    listener: UnixListener,
    shared: Arc<Mutex<Inner>>,
    channel: Channel,
    stopped: Arc<AtomicBool>,
) {
    let _ = thread::Builder::new()
        .name("cdct-accept".to_owned())
        .spawn(move || accept_loop(listener, shared, channel, stopped));
}

fn accept_loop(
    listener: UnixListener,
    shared: Arc<Mutex<Inner>>,
    channel: Channel,
    stopped: Arc<AtomicBool>,
) {
    for stream in listener.incoming() {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        // A transient accept error (descriptor exhaustion, an aborted
        // connection) must not end the listener.
        let Ok(stream) = stream else {
            continue;
        };
        let shared = Arc::clone(&shared);
        let channel = channel.clone();
        let _ = thread::Builder::new()
            .name("cdct-conn".to_owned())
            .spawn(move || serve_connection(shared, stream, channel));
    }
}

fn serve_connection(shared: Arc<Mutex<Inner>>, stream: UnixStream, channel: Channel) {
    let read_side = match stream.try_clone() {
        Ok(side) => side,
        Err(_) => return,
    };
    let mut reader = BufReader::new(read_side);
    let mut writer = stream;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > MAX_REQUEST_BYTES {
            let reply = Reply::Error {
                message: format!("a request exceeded {MAX_REQUEST_BYTES} bytes"),
            };
            if send(&mut writer, &reply).is_err() {
                return;
            }
            continue;
        }
        let request: Request = match decode(trimmed) {
            Ok(request) => request,
            Err(error) => {
                let reply = Reply::Error {
                    message: format!("invalid request: {error}"),
                };
                if send(&mut writer, &reply).is_err() {
                    return;
                }
                continue;
            }
        };
        match request {
            Request::Subscribe => {
                subscribe(Arc::clone(&shared), writer, channel);
                return;
            }
            Request::Status => {
                let reply = match lock(&shared).status() {
                    Ok(status) => Reply::Status { status },
                    Err(message) => Reply::Error { message },
                };
                if send(&mut writer, &reply).is_err() {
                    return;
                }
            }
            Request::PlanSwitch { target } => {
                let reply = match lock(&shared).plan(&target) {
                    Ok(outcome) => Reply::Plan { outcome },
                    Err(message) => Reply::Error { message },
                };
                if send(&mut writer, &reply).is_err() {
                    return;
                }
            }
            Request::Switch { target } => {
                let reply = {
                    let mut inner = lock(&shared);
                    match inner.switch(&target) {
                        Ok((outcome, events)) => {
                            inner.publish(&events);
                            Reply::Switch { outcome }
                        }
                        Err(message) => Reply::Error { message },
                    }
                };
                if send(&mut writer, &reply).is_err() {
                    return;
                }
            }
        }
    }
}

fn subscribe(shared: Arc<Mutex<Inner>>, mut writer: UnixStream, channel: Channel) {
    let (sender, receiver) = mpsc::channel::<String>();
    let initial = {
        let mut inner = lock(&shared);
        // Register before taking the snapshot, under the same lock, so no
        // change can fall between the snapshot and the subscription.
        inner.subscribers.push(sender);
        inner.status()
    };
    let first = match initial {
        Ok(status) => Reply::Subscribed { channel, status },
        Err(message) => {
            let _ = send(&mut writer, &Reply::Error { message });
            return;
        }
    };
    if send(&mut writer, &first).is_err() {
        return;
    }
    for line in receiver {
        if write_line(&mut writer, &line).is_err() {
            break;
        }
    }
}

fn lock(shared: &Arc<Mutex<Inner>>) -> MutexGuard<'_, Inner> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn send(writer: &mut UnixStream, reply: &Reply) -> io::Result<()> {
    let line = encode(reply).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_line(writer, &line)
}

fn write_line(writer: &mut UnixStream, line: &str) -> io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn now() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    Timestamp::from_millis(millis)
}

/// A daemon start failure.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The holder table could not be opened, locked or read.
    #[error("{0}")]
    Store(#[from] crate::holders::StoreError),
    /// A socket could not be bound or restricted.
    #[error("socket {path}: {source}")]
    Socket {
        /// The socket path.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// An inherited socket could not be taken from `LISTEN_FDS`.
    #[error("cannot take the socket systemd passed: {0}")]
    Activation(io::Error),
}
