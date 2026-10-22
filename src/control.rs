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

use crate::{app::App, wake};

const BOUND: Duration = Duration::from_secs(2);
const LIMIT: usize = 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct Registration {
    repo: PathBuf,
    server: Option<PathBuf>,
    address: SocketAddr,
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

    pub(crate) fn apply(self, app: &mut App) -> bool {
        let reply = if self.expired() {
            if self.wants_focus() {
                Reply::Refused("file request expired; no navigation performed".into())
            } else {
                Reply::Unknown
            }
        } else if app.repo.canonicalize().ok().as_ref() != Some(&self.repo) {
            Reply::Refused("review checkout changed".into())
        } else {
            match self.action {
                Action::Identify => Reply::Identified {
                    identity: Identity::of(app),
                    lease_ms: (self.epoch.elapsed() + BOUND).as_millis() as u64,
                },
                Action::Focus { expected, path, .. } => {
                    if expected != Identity::of(app) {
                        Reply::Refused("review pane moved; run the command again".into())
                    } else if let Err(error) = app.focus_file(&path) {
                        Reply::Refused(error.to_string())
                    } else {
                        Reply::Applied
                    }
                }
            }
        };
        let applied = matches!(reply, Reply::Applied);
        let _ = self.answer.send(reply);
        applied
    }
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
        let mut registration = tempfile::Builder::new()
            .prefix("launch-")
            .suffix(".json")
            .rand_bytes(32)
            .tempfile_in(dir)?;
        serde_json::to_writer(registration.as_file_mut(), &registration_data)?;
        registration.as_file_mut().flush()?;
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
    let relative = normalized.strip_prefix(repo).context("file is outside the checkout")?;
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
        thread::scope(|scope| {
            let client =
                scope.spawn(|| focus_in(&cache.path().join("control"), &root, None, None, "b.rs"));
            let identify = rx.recv_timeout(BOUND).unwrap();
            assert!(!identify.wants_focus());
            identify.apply(&mut app);
            let focus = rx.recv_timeout(BOUND).unwrap();
            assert!(focus.wants_focus());
            assert!(!client.is_finished(), "no applied acknowledgment before navigation");
            assert!(focus.apply(&mut app));
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
            let handle = thread::spawn(move || {
                while let Ok(request) = rx.recv() {
                    request.apply(&mut app);
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
        assert!(!expired.apply(&mut app));
        assert!(matches!(rx.recv().unwrap(), Reply::Refused(why) if why.contains("expired")));
        let (moved, rx) = pending(
            Identity { workspace: Some("previous".into()), pane: None },
            Instant::now() + BOUND,
        );
        assert!(!moved.apply(&mut app));
        assert!(matches!(rx.recv().unwrap(), Reply::Refused(why) if why.contains("moved")));
        let (blocked, rx) = pending(Identity::of(&app), Instant::now() + BOUND);
        app.set_config_error("bad config".into());
        assert!(!blocked.apply(&mut app));
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
        let mut app = app(repo.path());
        let queued = rx.recv_timeout(BOUND).unwrap();
        assert!(queued.expired());
        assert!(!queued.apply(&mut app));
        assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
    }

    #[test]
    fn focus_control_authenticates_and_honors_the_lease_before_accept() {
        let repo = repo();
        let cache = tempfile::tempdir().unwrap();
        let (tx, rx) = wake::channel(&wake::Waker::detached());
        let listener =
            Listener::in_dir(repo.path(), None, &cache.path().join("control"), tx).unwrap();
        let mut app = app(repo.path());
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
        assert!(!pending.apply(&mut app));
        assert!(!matches!(
            read_json::<Reply>(&mut stream, Instant::now() + BOUND).unwrap(),
            Reply::Applied
        ));
        assert_eq!(app.diff_path.as_deref(), Some("a.rs"));
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
            std::os::unix::fs::symlink("/outside", root.join("link.rs")).unwrap();
            assert_eq!(relative_path(&root, &root, Path::new("link.rs")).unwrap(), "link.rs");
        }
    }
}
