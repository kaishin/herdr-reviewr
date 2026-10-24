//! Local, authenticated file picks; the frame loop alone owns navigation.

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    app::{App, FocusFileInput, PreparedFocusFile},
    wake,
};

const BOUND: Duration = Duration::from_secs(2);
const LIMIT: usize = 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct Registration {
    repo: PathBuf,
    server: Option<PathBuf>,
    address: SocketAddr,
}

impl Registration {
    fn publish(&self, dir: &Path) -> Result<tempfile::NamedTempFile> {
        let mut staging =
            tempfile::Builder::new().prefix(".staging-").rand_bytes(32).tempfile_in(dir)?;
        serde_json::to_writer(staging.as_file_mut(), self)?;
        staging.as_file_mut().flush()?;
        let random = staging.path().file_name().context("registration name")?.to_string_lossy();
        let path = dir.join(format!("launch-{}.json", random.trim_start_matches(".staging-")));
        let file = staging.persist_noclobber(&path)?;
        Ok(tempfile::NamedTempFile::from_parts(file, tempfile::TempPath::try_from_path(path)?))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    workspace: Option<String>,
    pane: Option<String>,
}

impl Identity {
    fn of(app: &App) -> Self {
        Self { workspace: app.herdr.ids.workspace.clone(), pane: app.herdr.ids.pane.clone() }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    token: String,
    action: Action,
}

#[derive(Debug, Serialize, Deserialize)]
enum Action {
    Identify,
    Focus { expected: Identity, path: String, lease_ms: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
enum Reply {
    Identified { identity: Identity, lease_ms: u64 },
    Applied,
    Refused(String),
    Unknown,
}

/// A bounded request waiting for the UI owner, including while a terminal editor holds it.
#[derive(Debug)]
pub(crate) struct Pending {
    action: Action,
    repo: PathBuf,
    epoch: Instant,
    deadline: Instant,
    answer: mpsc::SyncSender<Reply>,
}

impl Pending {
    pub(crate) fn wants_focus(&self) -> bool {
        matches!(self.action, Action::Focus { .. })
    }

    pub(crate) fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    fn refusal(&self, app: &App) -> Option<Reply> {
        if self.expired() {
            if self.wants_focus() {
                Some(Reply::Refused("file request expired; no navigation performed".into()))
            } else {
                Some(Reply::Unknown)
            }
        } else if app.repo.canonicalize().ok().as_ref() != Some(&self.repo) {
            Some(Reply::Refused("review checkout changed".into()))
        } else if let Action::Focus { expected, .. } = &self.action
            && *expected != Identity::of(app)
        {
            Some(Reply::Refused("review pane moved; run the command again".into()))
        } else {
            None
        }
    }

    /// Identify at once, or capture a pick without changing the view and hand it to the worker.
    pub(crate) fn dispatch(self, app: &App, input_epoch: u64, tx: &mpsc::Sender<FocusJob>) -> bool {
        if let Some(reply) = self.refusal(app) {
            self.answer(reply);
            return false;
        }
        match &self.action {
            Action::Identify => {
                let reply = Reply::Identified {
                    identity: Identity::of(app),
                    lease_ms: (self.epoch.elapsed() + BOUND).as_millis() as u64,
                };
                self.answer(reply);
                false
            }
            Action::Focus { path, .. } => match app.prepare_focus_file(path) {
                Ok(input) => {
                    if let Err(error) = tx.send(FocusJob { pending: self, input, input_epoch }) {
                        error
                            .0
                            .pending
                            .answer(Reply::Refused("file preparation unavailable".into()));
                        false
                    } else {
                        true
                    }
                }
                Err(error) => {
                    self.answer(Reply::Refused(error.to_string()));
                    false
                }
            },
        }
    }

    fn answer(self, reply: Reply) -> bool {
        let applied = matches!(reply, Reply::Applied);
        let _ = self.answer.send(reply);
        applied
    }
}

#[derive(Debug)]
pub(crate) struct FocusJob {
    pending: Pending,
    input: FocusFileInput,
    input_epoch: u64,
}

impl FocusJob {
    fn build(self) -> FocusCompletion {
        let result = if self.pending.expired() {
            Err(anyhow::anyhow!("file request expired; no navigation performed"))
        } else {
            self.input.build()
        };
        FocusCompletion { pending: self.pending, input_epoch: self.input_epoch, result }
    }
}

#[derive(Debug)]
pub(crate) struct FocusCompletion {
    pending: Pending,
    input_epoch: u64,
    result: Result<PreparedFocusFile>,
}

impl FocusCompletion {
    pub(crate) fn expired(&self) -> bool {
        self.pending.expired()
    }

    pub(crate) fn apply(self, app: &mut App, input_epoch: u64) -> bool {
        let reply = if let Some(reply) = self.pending.refusal(app) {
            reply
        } else if self.input_epoch != input_epoch {
            Reply::Refused("review input changed while preparing file navigation".into())
        } else {
            match self.result.and_then(|prepared| app.focus_file(prepared)) {
                Ok(()) => Reply::Applied,
                Err(error) => Reply::Refused(error.to_string()),
            }
        };
        self.pending.answer(reply)
    }
}

/// One sleeping preparation worker; slow git never holds the listener or the frame loop.
pub(crate) fn prepare(
    rx: mpsc::Receiver<FocusJob>,
    tx: wake::Sender<FocusCompletion>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name("file-prepare".into()).spawn(move || {
        while let Ok(job) = rx.recv() {
            if tx.send(job.build()).is_err() {
                break;
            }
        }
    })
}

/// One launch's registration and sleeping listener; never polls or starts an idle process.
#[derive(Debug)]
pub(crate) struct Listener {
    registration: tempfile::NamedTempFile,
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Listener {
    pub(crate) fn start(repo: &Path, tx: wake::Sender<Pending>) -> Result<Self> {
        Self::in_dir(repo, server(), &crate::search::cache_dir().join("control"), tx)
    }

    fn in_dir(
        repo: &Path,
        server: Option<PathBuf>,
        dir: &Path,
        tx: wake::Sender<Pending>,
    ) -> Result<Self> {
        private_dir(dir)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;
        let repo = repo.canonicalize()?;
        let registration_data = Registration { repo: repo.clone(), server, address };
        let registration = registration_data.publish(dir)?;
        let token = registration
            .path()
            .file_name()
            .context("registration name")?
            .to_string_lossy()
            .into_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let epoch = Instant::now();
        let worker = thread::Builder::new().name("file-control".into()).spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    break;
                };
                let _ = serve(&mut stream, &token, &repo, epoch, &tx);
            }
        })?;
        Ok(Self { registration, address, stop, worker: Some(worker) })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = fs::remove_file(self.registration.path());
        let _ = TcpStream::connect_timeout(&self.address, BOUND);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir.parent().context("control cache parent")?)?;
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if let Err(error) = builder.create(dir) {
        ensure!(error.kind() == std::io::ErrorKind::AlreadyExists, "create control cache: {error}");
    }
    let metadata = fs::symlink_metadata(dir)?;
    ensure!(metadata.is_dir(), "control cache must be a directory, not a symlink");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode().trailing_zeros() >= 6,
            "control cache must be private (mode 0700)"
        );
    }
    Ok(())
}

fn serve(
    stream: &mut TcpStream,
    token: &str,
    repo: &Path,
    epoch: Instant,
    tx: &wake::Sender<Pending>,
) -> Result<()> {
    let mut deadline = Instant::now() + BOUND;
    let request: Request = read_json(stream, deadline)?;
    if request.token != token {
        return write_json(stream, &Reply::Refused("different review launch".into()), deadline);
    }
    if let Action::Focus { lease_ms, .. } = &request.action {
        // The launch-relative monotonic lease also expires requests queued before accept.
        deadline = deadline.min(
            epoch.checked_add(Duration::from_millis(*lease_ms)).context("invalid control lease")?,
        );
    }
    let (answer, rx) = mpsc::sync_channel(1);
    tx.send(Pending { action: request.action, repo: repo.to_owned(), epoch, deadline, answer })?;
    let reply = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(Reply::Unknown);
    write_json(stream, &reply, Instant::now() + BOUND)
}

fn read_json<T: DeserializeOwned>(stream: &mut TcpStream, deadline: Instant) -> Result<T> {
    let mut header = [0; 4];
    read_exact(stream, &mut header, deadline)?;
    let len = u32::from_be_bytes(header) as usize;
    ensure!(len <= LIMIT, "control message too large");
    let mut bytes = vec![0; len];
    read_exact(stream, &mut bytes, deadline)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_exact(stream: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "control read timed out");
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(bytes)?;
        ensure!(n > 0, "control endpoint closed");
        bytes = &mut bytes[n..];
    }
    Ok(())
}

fn write_json<T: Serialize>(stream: &mut TcpStream, value: &T, deadline: Instant) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= LIMIT, "control message too large");
    let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
    frame.extend(bytes);
    let mut bytes = frame.as_slice();
    while !bytes.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "control write timed out");
        stream.set_write_timeout(Some(remaining))?;
        let n = stream.write(bytes)?;
        ensure!(n > 0, "control endpoint closed");
        bytes = &bytes[n..];
    }
    Ok(())
}

fn server() -> Option<PathBuf> {
    crate::herdr::var_os("HERDR_SOCKET_PATH").map(PathBuf::from)
}

/// Ask an existing same-checkout review pane to select a cwd-relative file. Never opens a pane.
pub fn focus_file(path: &Path) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let repo = crate::git::toplevel(&cwd).context("not in a git checkout")?.canonicalize()?;
    let path = relative_path(&repo, &cwd, path)?;
    let workspace = crate::herdr::var("HERDR_WORKSPACE_ID");
    let server = server();
    ensure!(workspace.is_some() == server.is_some(), "incomplete Herdr workspace/server context");
    focus_in(
        &crate::search::cache_dir().join("control"),
        &repo,
        server.as_deref(),
        workspace.as_deref(),
        &path,
    )
}

fn relative_path(repo: &Path, cwd: &Path, path: &Path) -> Result<String> {
    let mut normalized = PathBuf::new();
    for component in cwd.join(path).components() {
        match component {
            Component::ParentDir => {
                ensure!(normalized.pop(), "file is outside the checkout");
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut ancestor = normalized.parent().context("expected a file path")?.to_owned();
    let mut suffix = vec![normalized.file_name().context("expected a file path")?.to_owned()];
    let resolved = loop {
        match ancestor.canonicalize() {
            Ok(mut parent) => {
                ensure!(parent.is_dir(), "file parent is not a directory");
                for name in suffix.iter().rev() {
                    parent.push(name);
                }
                break parent;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix
                    .push(ancestor.file_name().context("file is outside the checkout")?.to_owned());
                ensure!(ancestor.pop(), "file is outside the checkout");
            }
            Err(error) => return Err(error.into()),
        }
    };
    let relative = resolved.strip_prefix(repo).context("file is outside the checkout")?;
    ensure!(!relative.as_os_str().is_empty(), "expected a file, not the checkout root");
    relative
        .components()
        .map(|c| c.as_os_str().to_str().context("file path must be UTF-8"))
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

fn focus_in(
    dir: &Path,
    repo: &Path,
    server: Option<&Path>,
    workspace: Option<&str>,
    path: &str,
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("no existing reviewr pane for this checkout")
        }
        Err(error) => return Err(error.into()),
    };
    let mut targets = Vec::new();
    for entry in entries {
        let entry = entry?;
        let token = entry.file_name().to_string_lossy().into_owned();
        if !token.starts_with("launch-") || entry.path().extension().is_none_or(|ext| ext != "json")
        {
            continue;
        }
        let bytes = match fs::File::open(entry.path()) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take((LIMIT + 1) as u64).read_to_end(&mut bytes)?;
                bytes
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        ensure!(bytes.len() <= LIMIT, "invalid control registration");
        let registration: Registration = serde_json::from_slice(&bytes)
            .context("invalid control registration; cannot establish a unique target")?;
        if registration.repo != repo || registration.server.as_deref() != server {
            continue;
        }
        ensure!(
            registration.address.ip() == std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            "control endpoint is not loopback"
        );
        let Some(mut stream) = connect(&registration)? else {
            continue;
        };
        write_json(
            &mut stream,
            &Request { token: token.clone(), action: Action::Identify },
            Instant::now() + BOUND,
        )?;
        match read_json(&mut stream, Instant::now() + BOUND + Duration::from_secs(1))
            .context("review pane did not identify; cannot establish a unique target")?
        {
            Reply::Identified { identity, lease_ms }
                if identity.workspace.as_deref() == workspace =>
            {
                targets.push((registration, token, identity, lease_ms));
            }
            Reply::Identified { .. } | Reply::Refused(_) => {}
            _ => bail!("review pane unresponsive; cannot establish a unique target"),
        }
    }
    ensure!(!targets.is_empty(), "no existing reviewr pane for this checkout and workspace");
    ensure!(
        targets.len() == 1,
        "ambiguous target: multiple reviewr panes for this checkout and workspace"
    );
    let (registration, token, expected, lease_ms) = targets.pop().expect("one target");
    let mut stream = connect(&registration)?.context("review pane closed before navigation")?;
    write_json(
        &mut stream,
        &Request { token, action: Action::Focus { expected, path: path.into(), lease_ms } },
        Instant::now() + BOUND,
    )
    .context("file request interrupted; outcome unknown (no automatic retry)")?;
    match read_json(&mut stream, Instant::now() + BOUND + Duration::from_secs(1))
        .context("file request interrupted; outcome unknown (no automatic retry)")?
    {
        Reply::Applied => Ok(()),
        Reply::Refused(why) => bail!("{why}"),
        _ => bail!("file request timed out; outcome unknown (no automatic retry)"),
    }
}

fn connect(registration: &Registration) -> Result<Option<TcpStream>> {
    match TcpStream::connect_timeout(&registration.address, BOUND) {
        Ok(stream) => Ok(Some(stream)),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => Ok(None),
        Err(error) => {
            Err(error).context("review endpoint unavailable; cannot establish a unique target")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{Focus, Mode, Tab},
        herdr::PaneIds,
        model::Scope,
    };

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(dir.path().join("a.rs"), "a\n").unwrap();
        fs::write(dir.path().join("b.rs"), "b\n").unwrap();
        dir
    }

    fn app(repo: &Path) -> App {
        let mut app = App::new(repo.canonicalize().unwrap(), Scope::Uncommitted, None);
        app.herdr.ids = PaneIds::default();
        app.reload().unwrap();
        app
    }

    #[test]
    fn focus_control_acknowledges_only_after_owner_applies_and_cleans_up_on_quit() {
        let repo = repo();
        let cache = tempfile::tempdir().unwrap();
        let (tx, rx) = wake::channel(&wake::Waker::detached());
        let listener =
            Listener::in_dir(repo.path(), None, &cache.path().join("control"), tx).unwrap();
        fs::copy(listener.registration.path(), cache.path().join("control/launch-stale.json"))
            .unwrap();
        let mut app = app(repo.path());
        let root = app.repo.clone();
        let (job_tx, job_rx) = mpsc::channel();
        let (done_tx, done_rx) = wake::channel(&wake::Waker::detached());
        let _worker = prepare(job_rx, done_tx).unwrap();
        thread::scope(|scope| {
            let client =
                scope.spawn(|| focus_in(&cache.path().join("control"), &root, None, None, "b.rs"));
            let identify = rx.recv_timeout(BOUND).unwrap();
            assert!(!identify.wants_focus());
            assert!(!identify.dispatch(&app, 0, &job_tx));
            let focus = rx.recv_timeout(BOUND).unwrap();
            assert!(focus.wants_focus());
            assert!(!client.is_finished(), "no applied acknowledgment before navigation");
            let git_before = crate::git::GIT_COMMANDS.with(std::cell::Cell::get);
            assert!(focus.dispatch(&app, 0, &job_tx));
            assert_eq!(
                crate::git::GIT_COMMANDS.with(std::cell::Cell::get),
                git_before,
                "capturing a pick must not run git on the frame thread"
            );
            assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
            let completion = done_rx.recv_timeout(BOUND).unwrap();
            assert!(!client.is_finished(), "preparation is not an applied acknowledgment");
            assert!(completion.apply(&mut app, 0));
            client.join().unwrap().unwrap();
        });
        assert_eq!(app.current_entry().unwrap().path, "b.rs");
        assert_eq!(app.focus, Focus::Files);
        let registration = listener.registration.path().to_owned();
        let start = Instant::now();
        drop(listener);
        assert!(!registration.exists());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "idle shutdown must not wait for a timeout"
        );
    }

    #[test]
    fn focus_control_routes_by_live_workspace_and_refuses_ambiguity_or_no_target() {
        let repo = repo();
        let root = repo.path().canonicalize().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path().join("control");
        let server = Path::new("test-server");
        let owner = |workspace: &str| {
            let (tx, rx) = wake::channel(&wake::Waker::detached());
            let listener = Listener::in_dir(&root, Some(server.into()), &dir, tx).unwrap();
            let mut app = app(&root);
            app.herdr.ids =
                PaneIds { workspace: Some(workspace.into()), pane: Some("review-pane".into()) };
            let (job_tx, job_rx) = mpsc::channel();
            let (done_tx, done_rx) = wake::channel(&wake::Waker::detached());
            let _worker = prepare(job_rx, done_tx).unwrap();
            let handle = thread::spawn(move || {
                while let Ok(request) = rx.recv() {
                    if request.dispatch(&app, 0, &job_tx) {
                        done_rx.recv_timeout(BOUND).unwrap().apply(&mut app, 0);
                    }
                }
                app.diff_path.clone()
            });
            (listener, handle)
        };
        let (first, first_owner) = owner("other-workspace");
        let (second, second_owner) = owner("agent-workspace");
        focus_in(&dir, &root, Some(server), Some("agent-workspace"), "b.rs").unwrap();
        assert!(
            focus_in(&dir, &root, None, None, "a.rs")
                .unwrap_err()
                .to_string()
                .contains("no existing")
        );
        let (third, third_owner) = owner("agent-workspace");
        assert!(
            focus_in(&dir, &root, Some(server), Some("agent-workspace"), "a.rs")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        drop((first, second, third));
        assert_eq!(first_owner.join().unwrap().as_deref(), Some("a.rs"));
        assert_eq!(second_owner.join().unwrap().as_deref(), Some("b.rs"));
        assert_eq!(third_owner.join().unwrap().as_deref(), Some("a.rs"));
    }

    #[test]
    fn focus_control_refuses_expired_moved_and_blocked_requests_without_navigation() {
        let repo = repo();
        let mut app = app(repo.path());
        let root = app.repo.clone();
        let (job_tx, _job_rx) = mpsc::channel();
        let pending = |expected, deadline| {
            let (answer, rx) = mpsc::sync_channel(1);
            (
                Pending {
                    action: Action::Focus { expected, path: "b.rs".into(), lease_ms: 0 },
                    repo: root.clone(),
                    epoch: Instant::now(),
                    deadline,
                    answer,
                },
                rx,
            )
        };
        let (expired, rx) = pending(Identity::of(&app), Instant::now());
        assert!(!expired.dispatch(&app, 0, &job_tx));
        assert!(matches!(rx.recv().unwrap(), Reply::Refused(why) if why.contains("expired")));
        let (moved, rx) = pending(
            Identity { workspace: Some("previous".into()), pane: None },
            Instant::now() + BOUND,
        );
        assert!(!moved.dispatch(&app, 0, &job_tx));
        assert!(matches!(rx.recv().unwrap(), Reply::Refused(why) if why.contains("moved")));
        let (blocked, rx) = pending(Identity::of(&app), Instant::now() + BOUND);
        app.set_config_error("bad config".into());
        assert!(!blocked.dispatch(&app, 0, &job_tx));
        assert!(matches!(rx.recv().unwrap(), Reply::Refused(why) if why.contains("config")));
        assert_eq!(app.tab, Tab::Changes);
        assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn focus_control_unresponsive_candidate_is_not_absent_and_expired_queue_never_applies() {
        let repo = repo();
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path().join("control");
        let (tx, rx) = wake::channel(&wake::Waker::detached());
        let _listener = Listener::in_dir(repo.path(), None, &dir, tx).unwrap();
        let error =
            focus_in(&dir, &repo.path().canonicalize().unwrap(), None, None, "b.rs").unwrap_err();
        assert!(error.to_string().contains("unique target"));
        let app = app(repo.path());
        let queued = rx.recv_timeout(BOUND).unwrap();
        assert!(queued.expired());
        let (job_tx, _job_rx) = mpsc::channel();
        assert!(!queued.dispatch(&app, 0, &job_tx));
        assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
    }

    #[test]
    fn focus_control_authenticates_and_honors_the_lease_before_accept() {
        let repo = repo();
        let cache = tempfile::tempdir().unwrap();
        let (tx, rx) = wake::channel(&wake::Waker::detached());
        let listener =
            Listener::in_dir(repo.path(), None, &cache.path().join("control"), tx).unwrap();
        let app = app(repo.path());
        let mut stream = TcpStream::connect(listener.address).unwrap();
        write_json(
            &mut stream,
            &Request { token: "wrong".into(), action: Action::Identify },
            Instant::now() + BOUND,
        )
        .unwrap();
        assert!(matches!(
            read_json::<Reply>(&mut stream, Instant::now() + BOUND).unwrap(),
            Reply::Refused(_)
        ));
        assert!(rx.try_recv().is_err(), "unauthenticated requests never reach the owner");

        let token = listener.registration.path().file_name().unwrap().to_str().unwrap().to_owned();
        let mut stream = TcpStream::connect(listener.address).unwrap();
        write_json(
            &mut stream,
            &Request {
                token,
                action: Action::Focus {
                    expected: Identity::of(&app),
                    path: "b.rs".into(),
                    lease_ms: 0,
                },
            },
            Instant::now() + BOUND,
        )
        .unwrap();
        let pending = rx.recv_timeout(BOUND).unwrap();
        assert!(pending.expired(), "the lease cannot restart when the listener accepts");
        let (job_tx, _job_rx) = mpsc::channel();
        assert!(!pending.dispatch(&app, 0, &job_tx));
        assert!(!matches!(
            read_json::<Reply>(&mut stream, Instant::now() + BOUND).unwrap(),
            Reply::Applied
        ));
        assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
    }

    #[test]
    fn focus_control_prepared_pick_refuses_changed_owner_input_without_overwriting_place() {
        let repo = repo();
        let config_dir = tempfile::tempdir().unwrap();
        fs::write(config_dir.path().join("config.toml"), "review_prefix = 'changed'\n").unwrap();
        let changed_config = crate::config::plugin_config_in(config_dir.path()).unwrap();
        for change in [
            "expired",
            "pane",
            "repo",
            "config",
            "modal",
            "selection",
            "gesture",
            "navigation",
            "scope",
            "tab",
            "watch-worktree",
            "watch-head",
            "watch-refs",
            "watch-rescan",
            "watch-config",
            "watch-ready",
            "watch-unavailable",
            "watch-empty",
        ] {
            fs::write(repo.path().join("b.rs"), "b\n").unwrap();
            let mut app = app(repo.path());
            let (answer, reply) = mpsc::sync_channel(1);
            let pending = Pending {
                action: Action::Focus {
                    expected: Identity::of(&app),
                    path: "b.rs".into(),
                    lease_ms: 0,
                },
                repo: app.repo.clone(),
                epoch: Instant::now(),
                deadline: Instant::now() + BOUND,
                answer,
            };
            let (job_tx, job_rx) = mpsc::channel();
            assert!(pending.dispatch(&app, 7, &job_tx));
            assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
            assert!(reply.try_recv().is_err());
            let mut completion = job_rx.recv().unwrap().build();
            let mut input_epoch = 7;
            match change {
                "pane" => app.herdr.ids.workspace = Some("moved".into()),
                "repo" => app.repo = config_dir.path().to_owned(),
                "config" => app.set_plugin_config(changed_config.clone()),
                "modal" => {
                    app.focus = Focus::Diff;
                    app.start_comment();
                    app.input_push('x');
                    assert!(app.composing());
                }
                "selection" => app.select_anchor = Some(0),
                "gesture" => {
                    app.start_gutter_drag(0);
                    assert!(app.gesture_active());
                }
                "navigation" => {
                    app.diff_scroll = 3;
                    input_epoch += 1;
                }
                "scope" => app.scope = Scope::Branch,
                "tab" => app.tab = Tab::Pr,
                watch if watch.starts_with("watch-") => {
                    use crate::watch::{Batch, GitChange, WatchEvent};
                    let event = match watch {
                        "watch-ready" => WatchEvent::Ready,
                        "watch-unavailable" => WatchEvent::Unavailable("disconnected".into()),
                        _ => {
                            let mut batch = Batch::default();
                            match watch {
                                "watch-worktree" => {
                                    batch.worktree.insert("b.rs".into());
                                }
                                "watch-head" => {
                                    batch.git.insert(GitChange::Head);
                                }
                                "watch-refs" => {
                                    batch.git.insert(GitChange::Refs);
                                }
                                "watch-rescan" => batch.rescan = true,
                                "watch-config" => batch.config = true,
                                "watch-empty" => {}
                                _ => unreachable!(),
                            }
                            WatchEvent::Batch(batch)
                        }
                    };
                    crate::land_watch_event(
                        event,
                        &app,
                        &mut crate::schedule::Pacer::default(),
                        &mut crate::PrCoordinator::new(false),
                        None,
                        &mut input_epoch,
                    );
                    if watch != "watch-empty" {
                        fs::remove_file(repo.path().join("b.rs")).unwrap();
                        app.reload().unwrap();
                    }
                }
                _ => {}
            }
            // The owner is free to accept input before a queued build completes (Continuity).
            if change == "expired" {
                completion.pending.deadline = Instant::now();
            }
            if change == "watch-empty" {
                assert!(completion.apply(&mut app, input_epoch));
                assert!(matches!(reply.recv().unwrap(), Reply::Applied));
                assert_eq!(app.diff_path.as_deref(), Some("b.rs"));
                continue;
            }
            let place = (
                app.tab,
                app.scope,
                app.diff_path.clone(),
                app.diff_scroll,
                app.mode.clone(),
                app.input.clone(),
            );
            assert!(!completion.apply(&mut app, input_epoch), "{change}");
            assert!(matches!(reply.recv().unwrap(), Reply::Refused(_)), "{change}");
            assert_eq!(
                (
                    app.tab,
                    app.scope,
                    app.diff_path.clone(),
                    app.diff_scroll,
                    app.mode.clone(),
                    app.input.clone()
                ),
                place,
                "{change}"
            );
            if change.starts_with("watch-") {
                assert_eq!(app.changed_count(), 1, "{change}");
                assert!(!app.entries.iter().any(|entry| entry.path == "b.rs"), "{change}");
            }
        }
    }

    #[test]
    fn focus_control_publishes_complete_private_registration_and_ignores_unpublished_staging() {
        let repo = repo();
        let root = repo.path().canonicalize().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::write(cache.path().join(".staging-crashed"), "{\"repo\":").unwrap();
        assert!(
            focus_in(cache.path(), &root, None, None, "a.rs")
                .unwrap_err()
                .to_string()
                .contains("no existing")
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let data = Registration {
            repo: root.clone(),
            server: None,
            address: listener.local_addr().unwrap(),
        };
        let registration = data.publish(cache.path()).unwrap();
        let path = registration.path().to_owned();
        assert!(path.file_name().unwrap().to_str().unwrap().starts_with("launch-"));
        let published: Registration = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(published.repo, root);
        assert_eq!(published.address, listener.local_addr().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        drop(registration);
        assert!(!path.exists());
        fs::write(cache.path().join("launch-malformed.json"), "{").unwrap();
        assert!(
            focus_in(cache.path(), &root, None, None, "a.rs")
                .unwrap_err()
                .to_string()
                .contains("invalid control registration")
        );
    }

    #[test]
    fn focus_control_paths_are_lexical_cwd_relative_and_confined_to_checkout() {
        let repo = repo();
        let root = repo.path().canonicalize().unwrap();
        fs::create_dir(root.join("src")).unwrap();
        assert_eq!(relative_path(&root, &root.join("src"), Path::new("../b.rs")).unwrap(), "b.rs");
        assert!(relative_path(&root, &root, Path::new("../outside.rs")).is_err());
        #[cfg(unix)]
        {
            let aliases = tempfile::tempdir().unwrap();
            let alias = aliases.path().join("checkout");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            assert_eq!(relative_path(&root, &root, &alias.join("a.rs")).unwrap(), "a.rs");
            fs::write(root.join("deleted.rs"), "deleted").unwrap();
            fs::remove_file(root.join("deleted.rs")).unwrap();
            assert_eq!(
                relative_path(&root, &root, &alias.join("deleted.rs")).unwrap(),
                "deleted.rs"
            );
            assert_eq!(
                relative_path(&root, &root, &alias.join("gone/deleted.rs")).unwrap(),
                "gone/deleted.rs"
            );
            fs::write(aliases.path().join("outside.rs"), "outside").unwrap();
            std::os::unix::fs::symlink(aliases.path().join("outside.rs"), root.join("link.rs"))
                .unwrap();
            assert_eq!(relative_path(&root, &root, &alias.join("link.rs")).unwrap(), "link.rs");
            std::os::unix::fs::symlink(aliases.path(), root.join("escape")).unwrap();
            assert!(relative_path(&root, &root, &alias.join("escape/outside.rs")).is_err());
            assert!(relative_path(&root, &root, &alias.join("../outside.rs")).is_err());
        }
    }
}
