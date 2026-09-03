use std::{
    any::Any,
    cell::Cell,
    collections::HashMap,
    env,
    ffi::OsString,
    fmt, fs,
    num::NonZeroU32,
    panic,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
        Arc, LazyLock, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

use regex::Regex;
use tempfile::TempDir;

mod command;
mod git_interceptor;
mod mock_server;

pub use command::TestCommand;

pub const DEFAULT_OWNER: &str = "owner";
pub const DEFAULT_REPO: &str = "repo";
pub const MANAGED_PRIVATE: &str = "managedPrivate";
pub const MANAGED_PUBLIC: &str = "managedPublic";

const FIRST_GIT_TIMESTAMP: u64 = 946_684_800;
const MOCK_SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const MOCK_SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const PUBLICATION_OVERLAP_TIMEOUT: Duration = Duration::from_secs(30);

#[macro_export]
macro_rules! test_context {
    () => {
        $crate::TestContextBuilder::new(assert_cmd::cargo::cargo_bin!("gherrit-test-driver"))
    };
}

pub struct TestContextBuilder {
    owner: String,
    name: String,
    remote: bool,
    installed_hooks: bool,
    initial_commit: bool,
    mock_github: bool,
    git_interceptor: bool,
    publication_overlap: bool,
    gherrit_bin: PathBuf,
}

impl TestContextBuilder {
    pub fn new(gherrit_bin: impl Into<PathBuf>) -> Self {
        Self {
            owner: DEFAULT_OWNER.to_string(),
            name: DEFAULT_REPO.to_string(),
            remote: false,
            installed_hooks: false,
            initial_commit: false,
            mock_github: false,
            git_interceptor: false,
            publication_overlap: false,
            gherrit_bin: gherrit_bin.into(),
        }
    }

    #[must_use]
    pub fn repository(mut self, owner: &str, name: &str) -> Self {
        self.owner = owner.to_string();
        self.name = name.to_string();
        self
    }

    #[must_use]
    pub fn with_remote(mut self) -> Self {
        self.remote = true;
        self
    }

    #[must_use]
    pub fn with_installed_hooks(mut self) -> Self {
        self.installed_hooks = true;
        self
    }

    #[must_use]
    pub fn with_initial_commit(mut self) -> Self {
        self.initial_commit = true;
        self
    }

    #[must_use]
    pub fn with_mock_github(mut self) -> Self {
        self.mock_github = true;
        self
    }

    #[must_use]
    pub fn with_git_interceptor(mut self) -> Self {
        self.git_interceptor = true;
        self
    }

    /// Installs deterministic gates around concurrent create and marker
    /// boundaries. The controller remains outside mock state and its locks.
    #[must_use]
    pub fn with_publication_overlap(mut self) -> Self {
        self.publication_overlap = true;
        self.mock_github = true;
        self.git_interceptor = true;
        self
    }

    pub fn build(self) -> TestContext {
        let dir = Arc::new(TempDir::new().unwrap());
        let system_git = SYSTEM_GIT.clone();
        let test_environment = TestEnvironment::new(dir.path(), &system_git);
        let repo_path = dir.path().join("local");
        fs::create_dir(&repo_path).unwrap();

        let remote_parent = dir.path().join(&self.owner);
        let remote_path = remote_parent.join(format!("{}.git", self.name));
        if self.remote {
            fs::create_dir_all(&remote_parent).unwrap();
            init_git_bare_repo(&test_environment, &system_git, &remote_path);
        }

        init_git_repo(
            &test_environment,
            &system_git,
            &repo_path,
            self.remote.then_some(remote_path.as_path()),
        );

        if self.installed_hooks {
            install_gherrit_binary(dir.path(), &self.gherrit_bin);
        }
        if self.git_interceptor {
            install_git_interceptor(dir.path(), &self.gherrit_bin);
        }

        let mut mock_server_state = None;
        let (overlap_schedule, publication_overlap) = if self.publication_overlap {
            let (schedule, controller) = PublicationOverlapSchedule::new();
            (Some(schedule), Some(controller))
        } else {
            (None, None)
        };

        let mock_server = (self.mock_github || self.git_interceptor).then(|| {
            let state = mock_server::MockState::new(self.owner.clone(), self.name.clone());
            let state = Arc::new(RwLock::new(state));
            mock_server_state = Some(state.clone());

            MockServerInfo::start(
                state,
                remote_path.clone(),
                system_git.clone(),
                test_environment.clone(),
                dir.clone(),
                overlap_schedule,
            )
        });

        let ctx = TestContext {
            dir,
            repo_path,
            remote_path,
            has_remote: self.remote,
            has_mock_github: self.mock_github,
            has_git_interceptor: self.git_interceptor,
            system_git: system_git.clone(),
            gherrit_bin_path: self.gherrit_bin,
            test_environment,
            next_git_timestamp: AtomicU64::new(FIRST_GIT_TIMESTAMP),
            next_gherrit_id: AtomicU64::new(1),
            mock_server,
            mock_server_state,
            publication_overlap,
        };

        if self.installed_hooks {
            ctx.gherrit_cmd().arg("install").assert().success();
        }

        if self.initial_commit {
            ctx.commit("Initial commit");
            if self.remote {
                ctx.seed_remote_main();
            }
        }

        ctx
    }
}

pub struct TestContext {
    pub dir: Arc<TempDir>,
    pub repo_path: PathBuf,
    pub system_git: PathBuf,
    pub gherrit_bin_path: PathBuf,
    remote_path: PathBuf,
    has_remote: bool,
    has_mock_github: bool,
    has_git_interceptor: bool,
    test_environment: TestEnvironment,
    next_git_timestamp: AtomicU64,
    next_gherrit_id: AtomicU64,
    mock_server: Option<MockServerInfo>,
    mock_server_state: Option<Arc<RwLock<mock_server::MockState>>>,
    publication_overlap: Option<PublicationOverlapController>,
}

#[derive(Clone)]
struct TestEnvironment {
    variables: Vec<(OsString, OsString)>,
}

impl TestEnvironment {
    fn new(root: &Path, system_git: &Path) -> Self {
        let home = root.join("home");
        let temporary_directory = root.join("tmp");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&temporary_directory).unwrap();

        let git_config = home.join("gitconfig");
        // Put the fixture identity in the per-context global config so every
        // repository and clone created by the test is deterministic.
        fs::write(
            &git_config,
            concat!(
                "[color]\n",
                "\tui = false\n",
                "[commit]\n",
                "\tgpgSign = false\n",
                "[tag]\n",
                "\tgpgSign = false\n",
                "[user]\n",
                "\temail = test@example.com\n",
                "\tname = Test User\n",
            ),
        )
        .unwrap();

        let mut paths = vec![root.to_path_buf()];
        if let Some(parent) = system_git.parent() {
            push_unique_path(&mut paths, parent);
        }

        #[cfg(unix)]
        for path in ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
            push_unique_path(&mut paths, Path::new(path));
        }

        let mut variables = vec![
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("XDG_CONFIG_HOME"), home.join(".config").into_os_string()),
            (OsString::from("TMPDIR"), temporary_directory.clone().into_os_string()),
            (OsString::from("TMP"), temporary_directory.clone().into_os_string()),
            (OsString::from("TEMP"), temporary_directory.into_os_string()),
            (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_CONFIG_GLOBAL"), git_config.into_os_string()),
            (OsString::from("GIT_ATTR_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
            (OsString::from("GCM_INTERACTIVE"), OsString::from("never")),
            (OsString::from("GIT_PAGER"), OsString::from("cat")),
            (OsString::from("PAGER"), OsString::from("cat")),
            (OsString::from("GIT_EDITOR"), OsString::from("true")),
            (OsString::from("GIT_SEQUENCE_EDITOR"), OsString::from("true")),
            (OsString::from("LANG"), OsString::from("C")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("TZ"), OsString::from("UTC")),
            (OsString::from("TERM"), OsString::from("dumb")),
            (OsString::from("NO_COLOR"), OsString::from("1")),
            (OsString::from("RUST_LOG"), OsString::from("info")),
            (OsString::from("RUST_BACKTRACE"), OsString::from("0")),
            (OsString::from("NO_PROXY"), OsString::from("127.0.0.1,localhost")),
            (OsString::from("no_proxy"), OsString::from("127.0.0.1,localhost")),
        ];

        // Preserve only process-instrumentation settings needed to run the
        // already-built test binaries. These do not provide developer or CI
        // configuration to GHerrit, but dropping them would make coverage and
        // sanitizer runs silently incomplete or unable to start.
        for name in [
            "LLVM_PROFILE_FILE",
            "ASAN_OPTIONS",
            "LSAN_OPTIONS",
            "MSAN_OPTIONS",
            "TSAN_OPTIONS",
            "UBSAN_OPTIONS",
        ] {
            if let Some(value) = env::var_os(name) {
                variables.push((OsString::from(name), value));
            }
        }

        #[cfg(target_os = "linux")]
        if let Some(value) = env::var_os("LD_LIBRARY_PATH") {
            variables.push((OsString::from("LD_LIBRARY_PATH"), value));
        }

        #[cfg(target_os = "macos")]
        for name in ["DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
            if let Some(value) = env::var_os(name) {
                variables.push((OsString::from(name), value));
            }
        }

        #[cfg(windows)]
        {
            if let Some(system_root) = env::var_os("SystemRoot") {
                let system_root = PathBuf::from(system_root);
                push_unique_path(&mut paths, &system_root.join("System32"));
                variables
                    .push((OsString::from("SystemRoot"), system_root.clone().into_os_string()));
                variables.push((OsString::from("WINDIR"), system_root.clone().into_os_string()));
                let command_processor = env::var_os("COMSPEC")
                    .unwrap_or_else(|| system_root.join("System32/cmd.exe").into_os_string());
                variables.push((OsString::from("COMSPEC"), command_processor));
            }

            if let Some(git_root) = system_git.parent().and_then(Path::parent) {
                push_unique_path(&mut paths, &git_root.join("mingw64/bin"));
                push_unique_path(&mut paths, &git_root.join("usr/bin"));
            }
            variables.push((OsString::from("PATHEXT"), OsString::from(".COM;.EXE;.BAT;.CMD")));
        }

        variables.push((OsString::from("PATH"), env::join_paths(paths).unwrap()));
        Self { variables }
    }

    fn apply_to_command(&self, cmd: &mut TestCommand) {
        cmd.env_clear();
        cmd.envs(self.variables.iter().cloned());
    }

    fn variable(&self, name: &str) -> &OsString {
        self.variables
            .iter()
            .find_map(|(key, value)| (key == name).then_some(value))
            .unwrap_or_else(|| panic!("test environment does not define {name}"))
    }

    fn command(&self, program: &Path) -> TestCommand {
        let mut cmd = TestCommand::new(program);
        self.apply_to_command(&mut cmd);
        cmd
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: &Path) {
    if !paths.iter().any(|existing| existing == path) {
        paths.push(path.to_path_buf());
    }
}

pub struct MockServerInfo {
    pub url: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    completion_rx: Option<Receiver<thread::Result<()>>>,
}

enum MockServerStopError {
    Panicked(Box<dyn Any + Send + 'static>),
    TimedOut(Duration),
    Disconnected,
}

impl fmt::Display for MockServerStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panicked(_) => formatter.write_str("mock server thread panicked"),
            Self::TimedOut(timeout) => {
                write!(formatter, "mock server did not stop within {timeout:?}")
            }
            Self::Disconnected => {
                formatter.write_str("mock server thread exited without reporting its result")
            }
        }
    }
}

impl MockServerInfo {
    fn start(
        state: Arc<RwLock<mock_server::MockState>>,
        remote_path: PathBuf,
        system_git: PathBuf,
        test_environment: TestEnvironment,
        fixture_dir: Arc<TempDir>,
        publication_overlap: Option<Arc<PublicationOverlapSchedule>>,
    ) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (completion_tx, completion_rx) = mpsc::channel();

        thread::Builder::new()
            .name("gherrit-mock-server".to_string())
            .spawn(move || {
                let result = panic::catch_unwind(panic::AssertUnwindSafe(move || {
                    // Keep the fixture directory alive if this thread must be
                    // detached after a pathological shutdown timeout.
                    let _fixture_dir = fixture_dir;
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to build mock server runtime");
                    runtime.block_on(mock_server::run_mock_server(
                        state,
                        remote_path,
                        system_git,
                        test_environment,
                        publication_overlap,
                        ready_tx,
                        shutdown_rx,
                    ));
                }));
                let _ = completion_tx.send(result);
            })
            .expect("Failed to spawn mock server thread");

        let url = match ready_rx.recv_timeout(MOCK_SERVER_STARTUP_TIMEOUT) {
            Ok(url) => url,
            Err(startup_error) => match Self::stop(Some(shutdown_tx), &completion_rx) {
                Ok(()) => panic!(
                    "Mock server did not become ready within \
                     {MOCK_SERVER_STARTUP_TIMEOUT:?}: {startup_error}"
                ),
                Err(MockServerStopError::Panicked(server_panic)) => {
                    panic::resume_unwind(server_panic)
                }
                Err(stop_error) => panic!(
                    "Mock server did not become ready within \
                     {MOCK_SERVER_STARTUP_TIMEOUT:?}: {startup_error}; {stop_error}"
                ),
            },
        };

        Self { url, shutdown_tx: Some(shutdown_tx), completion_rx: Some(completion_rx) }
    }

    fn stop(
        shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
        completion_rx: &Receiver<thread::Result<()>>,
    ) -> Result<(), MockServerStopError> {
        if let Some(shutdown_tx) = shutdown_tx {
            let _ = shutdown_tx.send(());
        }

        wait_for_server_completion(completion_rx, MOCK_SERVER_SHUTDOWN_TIMEOUT)
    }
}

fn wait_for_server_completion(
    completion_rx: &Receiver<thread::Result<()>>,
    timeout: Duration,
) -> Result<(), MockServerStopError> {
    match completion_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(server_panic)) => Err(MockServerStopError::Panicked(server_panic)),
        Err(RecvTimeoutError::Timeout) => Err(MockServerStopError::TimedOut(timeout)),
        Err(RecvTimeoutError::Disconnected) => Err(MockServerStopError::Disconnected),
    }
}

impl Drop for MockServerInfo {
    fn drop(&mut self) {
        let completion_rx = self.completion_rx.take().expect("mock server completion receiver");
        if let Err(error) = Self::stop(self.shutdown_tx.take(), &completion_rx) {
            if thread::panicking() {
                eprintln!("Mock server teardown also failed: {error}");
            } else if let MockServerStopError::Panicked(server_panic) = error {
                panic::resume_unwind(server_panic);
            } else {
                panic!("Mock server teardown failed: {error}");
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitOperation {
    Var,
    InterpretTrailers,
    LsRemote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryableHttpStatus {
    TooManyRequests,
    ServiceUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectStatus {
    Temporary,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedJson {
    DuplicateObjectMember,
    TrailingValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationPushStage {
    Initial,
    Marker,
}

/// Server-side gates for one deliberately scheduled two-publisher overlap.
///
/// Semaphores retain releases even if teardown wins a race with a handler.
/// Arrival channels let the synchronous test controller inspect durable state
/// without sleeping or entering the server's state lock.
pub(crate) struct PublicationOverlapSchedule {
    cancelled: AtomicBool,
    create_slots: AtomicUsize,
    create_arrived: mpsc::Sender<()>,
    create_application_release: Arc<tokio::sync::Semaphore>,
    create_applied: mpsc::Sender<()>,
    create_response_release: Arc<tokio::sync::Semaphore>,
    marker_arrived: mpsc::Sender<()>,
    marker_release: Arc<tokio::sync::Semaphore>,
    marker_slots: AtomicUsize,
}

impl PublicationOverlapSchedule {
    fn new() -> (Arc<Self>, PublicationOverlapController) {
        let (create_arrived, create_arrivals) = mpsc::channel();
        let (create_applied, create_applications) = mpsc::channel();
        let (marker_arrived, marker_arrivals) = mpsc::channel();
        let schedule = Arc::new(Self {
            cancelled: AtomicBool::new(false),
            create_slots: AtomicUsize::new(2),
            create_arrived,
            create_application_release: Arc::new(tokio::sync::Semaphore::new(0)),
            create_applied,
            create_response_release: Arc::new(tokio::sync::Semaphore::new(0)),
            marker_arrived,
            marker_release: Arc::new(tokio::sync::Semaphore::new(0)),
            marker_slots: AtomicUsize::new(2),
        });
        let controller = PublicationOverlapController {
            schedule: schedule.clone(),
            create_arrivals,
            create_applications,
            marker_arrivals,
            deadline: Cell::new(None),
        };
        (schedule, controller)
    }

    fn claim(slot_count: &AtomicUsize) -> bool {
        slot_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| remaining.checked_sub(1))
            .is_ok()
    }

    pub(crate) fn claim_create(&self) -> bool {
        Self::claim(&self.create_slots)
    }

    pub(crate) fn claim_marker(&self) -> bool {
        Self::claim(&self.marker_slots)
    }

    async fn wait_for_release(&self, release: &tokio::sync::Semaphore) -> bool {
        release.acquire().await.expect("overlap release semaphore remains open").forget();
        !self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn before_create_application(&self) -> bool {
        self.create_arrived.send(()).expect("overlap controller must receive create arrival");
        self.wait_for_release(&self.create_application_release).await
    }

    pub(crate) async fn after_create_application(&self) -> bool {
        self.create_applied.send(()).expect("overlap controller must receive applied create");
        self.wait_for_release(&self.create_response_release).await
    }

    pub(crate) async fn before_marker_push(&self) -> bool {
        self.marker_arrived.send(()).expect("overlap controller must receive marker arrival");
        self.wait_for_release(&self.marker_release).await
    }

    fn release_all(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.create_application_release.add_permits(2);
        self.create_response_release.add_permits(2);
        self.marker_release.add_permits(2);
    }
}

/// Synchronous control surface for the deterministic overlap schedule.
pub struct PublicationOverlapController {
    schedule: Arc<PublicationOverlapSchedule>,
    create_arrivals: Receiver<()>,
    create_applications: Receiver<()>,
    marker_arrivals: Receiver<()>,
    deadline: Cell<Option<Instant>>,
}

impl PublicationOverlapController {
    fn deadline(&self) -> Instant {
        self.deadline.get().expect("an overlap cancellation guard must be active")
    }

    fn wait_twice(&self, receiver: &Receiver<()>, boundary: &str) {
        for publisher in 1..=2 {
            let remaining = self.deadline().saturating_duration_since(Instant::now());
            receiver.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!("publisher {publisher} did not reach {boundary}: {error}")
            });
        }
    }

    pub fn wait_for_create_arrivals(&self) {
        self.wait_twice(&self.create_arrivals, "the create pre-application gate");
    }

    pub fn release_create_applications(&self) {
        self.schedule.create_application_release.add_permits(2);
    }

    pub fn wait_for_create_applications(&self) {
        self.wait_twice(&self.create_applications, "the create post-application gate");
    }

    pub fn release_create_responses(&self) {
        self.schedule.create_response_release.add_permits(2);
    }

    pub fn wait_for_marker_arrivals(&self) {
        self.wait_twice(&self.marker_arrivals, "the marker pre-push gate");
    }

    pub fn release_marker_pushes(&self) {
        self.schedule.marker_release.add_permits(2);
    }

    /// Starts one bounded schedule and returns its unwind cancellation guard.
    pub fn cancellation_guard(&self) -> PublicationOverlapCancellation<'_> {
        assert!(self.deadline.get().is_none(), "overlap schedule starts exactly once");
        self.deadline.set(Some(Instant::now() + PUBLICATION_OVERLAP_TIMEOUT));
        PublicationOverlapCancellation { schedule: &self.schedule, armed: true }
    }
}

/// Cancels blocked server handlers before scoped publisher threads are joined.
pub struct PublicationOverlapCancellation<'schedule> {
    schedule: &'schedule PublicationOverlapSchedule,
    armed: bool,
}

impl PublicationOverlapCancellation<'_> {
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PublicationOverlapCancellation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.schedule.release_all();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    GraphQl,
    QueryTransport,
    QueryHttp(RetryableHttpStatus),
    CreatePr,
    CreatePrApplyThenDisconnect,
    CreatePrMalformedJson(MalformedJson),
    CreatePrHttp(RetryableHttpStatus),
    CreatePrRedirect(RedirectStatus),
    ClosePr,
    ClosePrApplyThenDisconnect,
    UpdatePr,
    UpdatePrApplyThenDisconnect,
    UpdatePrMalformedJson(MalformedJson),
    UpdatePrConcurrentClose,
    LosePublicationPushReceipt(PublicationPushStage),
    Git(GitOperation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphQlOperation {
    Query,
    CreatePr,
    DraftPr,
    ClosePr,
    UpdatePr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSeed {
    number: NonZeroU32,
    title: String,
    body: String,
    head: String,
    base: String,
    is_draft: bool,
}

impl PullRequestSeed {
    pub fn new(
        number: usize,
        title: impl Into<String>,
        body: impl Into<String>,
        head: impl Into<String>,
        base: impl Into<String>,
    ) -> Self {
        let number = valid_pull_request_number(number)
            .expect("a seeded pull request number must be in 1..=GraphQL Int::MAX");
        Self {
            number,
            title: title.into(),
            body: body.into(),
            head: head.into(),
            base: base.into(),
            is_draft: false,
        }
    }

    pub fn draft(mut self) -> Self {
        self.is_draft = true;
        self
    }
}

fn valid_pull_request_number(number: usize) -> Option<NonZeroU32> {
    let number = u32::try_from(number).ok().and_then(NonZeroU32::new)?;
    (number.get() <= i32::MAX as u32).then_some(number)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PullRequestSnapshot {
    pub number: usize,
    pub node_id: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRecord {
    arguments: Vec<String>,
    pub exit_code: i32,
}

impl PushRecord {
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

impl From<&mock_server::PrEntry> for PullRequestSnapshot {
    fn from(pr: &mock_server::PrEntry) -> Self {
        Self {
            number: pr.number,
            node_id: pr.node_id.clone(),
            state: pr.state,
            is_draft: pr.is_draft,
            title: pr.title.clone(),
            body: pr.body.clone(),
            head: pr.head.name.clone(),
            base: pr.base.name.clone(),
        }
    }
}

pub struct MockGithub<'a> {
    context: &'a TestContext,
}

impl MockGithub<'_> {
    pub fn repository(&self) -> (String, String) {
        self.context.inspect_mock_state(|state| (state.repo_owner.clone(), state.repo_name.clone()))
    }

    pub fn pull_requests(&self) -> Vec<PullRequestSnapshot> {
        self.context
            .inspect_mock_state(|state| state.prs.iter().map(PullRequestSnapshot::from).collect())
    }

    pub fn requests(&self) -> Vec<Vec<GraphQlOperation>> {
        self.context.inspect_mock_state(|state| state.graphql_requests.clone())
    }

    pub fn redirect_trap_requests(&self) -> usize {
        self.context.inspect_mock_state(|state| state.graphql_redirect_trap_requests)
    }

    /// Overrides GitHub's view of the default branch without changing Git.
    pub fn set_default_branch(&self, name: &str, object_id: &str) {
        self.context.mutate_mock_state(|state| {
            state.github_default_branch = Some((name.to_owned(), object_id.to_owned()));
        });
    }

    pub fn seed_pull_request(&self, seed: PullRequestSeed) {
        let PullRequestSeed { number, title, body, head, base, is_draft } = seed;
        self.context.mutate_mock_state(|state| {
            let mut pr = mock_server::PrEntry::mock(mock_server::MockPrArgs {
                number: usize::try_from(number.get())
                    .expect("the test target must represent GraphQL pull request numbers"),
                title,
                body,
                head,
                base,
            });
            pr.is_draft = is_draft;
            state.add_pr(pr);
        });
    }

    /// Inserts a pull request before the existing connection rows.
    pub fn seed_earlier_pull_request(&self, seed: PullRequestSeed) {
        let PullRequestSeed { number, title, body, head, base, is_draft } = seed;
        self.context.mutate_mock_state(|state| {
            let mut pr = mock_server::PrEntry::mock(mock_server::MockPrArgs {
                number: usize::try_from(number.get())
                    .expect("the test target must represent GraphQL pull request numbers"),
                title,
                body,
                head,
                base,
            });
            pr.is_draft = is_draft;
            state.prs.insert(0, pr);
        });
    }

    /// Seeds an intentionally malformed GraphQL identity for rejection tests.
    pub fn seed_pull_request_with_invalid_number(
        &self,
        number: usize,
        title: impl Into<String>,
        body: impl Into<String>,
        head: impl Into<String>,
        base: impl Into<String>,
    ) {
        assert!(
            valid_pull_request_number(number).is_none(),
            "ordinary pull request numbers must use seed_pull_request"
        );
        self.context.mutate_mock_state(|state| {
            state.add_pr(mock_server::PrEntry::mock(mock_server::MockPrArgs {
                number,
                title: title.into(),
                body: body.into(),
                head: head.into(),
                base: base.into(),
            }));
        });
    }

    pub fn seed_cross_repository_pull_request(
        &self,
        seed: PullRequestSeed,
        head_oid: &str,
        base_oid: &str,
    ) {
        for (kind, oid) in [("head", head_oid), ("base", base_oid)] {
            assert!(
                oid.len() == 40 && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "cross-repository {kind} object ID must be 40 hexadecimal digits"
            );
        }
        let PullRequestSeed { number, title, body, head, base, is_draft } = seed;
        self.context.mutate_mock_state(|state| {
            let number = usize::try_from(number.get())
                .expect("the test target must represent GraphQL pull request numbers");
            let mut pr = mock_server::PrEntry::mock(mock_server::MockPrArgs {
                number,
                title,
                body,
                head,
                base,
            });
            pr.is_draft = is_draft;
            pr.head.oid = head_oid.to_owned();
            pr.base.oid = base_oid.to_owned();
            state.add_pr(pr);
            state.cross_repository_prs.insert(number);
        });
    }

    pub fn set_pull_request_landing_automation(
        &self,
        number: usize,
        auto_merge: bool,
        in_merge_queue: bool,
    ) {
        self.context.mutate_mock_state(|state| {
            let pr = state
                .prs
                .iter_mut()
                .find(|pr| pr.number == number)
                .unwrap_or_else(|| panic!("pull request #{number} does not exist"));
            pr.auto_merge = auto_merge;
            pr.in_merge_queue = in_merge_queue;
        });
    }

    pub fn set_pull_request_state(&self, number: usize, new_state: PullRequestState) {
        self.context.mutate_mock_state(|state| {
            let pr = state
                .prs
                .iter_mut()
                .find(|pr| pr.number == number)
                .unwrap_or_else(|| panic!("pull request #{number} does not exist"));
            pr.state = new_state;
        });
    }

    pub fn set_pull_request_draft(&self, number: usize, is_draft: bool) {
        self.context.mutate_mock_state(|state| {
            let pr = state
                .prs
                .iter_mut()
                .find(|pr| pr.number == number)
                .unwrap_or_else(|| panic!("pull request #{number} does not exist"));
            pr.is_draft = is_draft;
        });
    }

    pub fn set_pull_request_title(&self, number: usize, new_title: &str) {
        self.context.mutate_mock_state(|state| {
            let pr = state
                .prs
                .iter_mut()
                .find(|pr| pr.number == number)
                .unwrap_or_else(|| panic!("pull request #{number} does not exist"));
            pr.title = new_title.to_owned();
        });
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if let Some(overlap) = &self.publication_overlap {
            overlap.schedule.release_all();
        }
        // Stop the server before fixture directories and state are released.
        drop(self.mock_server.take());

        let (state_poisoned, pending_faults, pending_remote_ref_updates) = self
            .mock_server_state
            .as_ref()
            .map(|state| match state.read() {
                Ok(state) => (false, state.faults.clone(), state.git.pending_remote_ref_updates()),
                Err(poisoned) => {
                    let state = poisoned.into_inner();
                    (true, state.faults.clone(), state.git.pending_remote_ref_updates())
                }
            })
            .unwrap_or_default();
        if state_poisoned {
            let message = format!(
                "Test fixture mock state was poisoned; unconsumed faults: {pending_faults:?}; \
                 unconsumed scheduled remote ref updates: {pending_remote_ref_updates}"
            );
            if thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
            return;
        }
        if !pending_faults.is_empty() || pending_remote_ref_updates != 0 {
            if thread::panicking() {
                eprintln!(
                    "Test fixture also has unconsumed faults: {pending_faults:?}; \
                     unconsumed scheduled remote ref updates: {pending_remote_ref_updates}"
                );
            } else {
                panic!(
                    "Test fixture has unconsumed faults: {pending_faults:?}; \
                     unconsumed scheduled remote ref updates: {pending_remote_ref_updates}"
                );
            }
        }
    }
}

impl TestContext {
    fn configure_test_env(&self, cmd: &mut TestCommand) {
        self.test_environment.apply_to_command(cmd);
        self.configure_test_capabilities(cmd);
    }

    fn configure_test_capabilities(&self, cmd: &mut TestCommand) {
        // Give each command a deterministic, unique timestamp. In particular,
        // this ensures an otherwise-empty amend creates a distinct Git object.
        let timestamp = self.next_git_timestamp.fetch_add(1, Ordering::Relaxed);
        let git_date = format!("@{timestamp} +0000");
        cmd.env("GIT_AUTHOR_DATE", &git_date);
        cmd.env("GIT_COMMITTER_DATE", &git_date);

        if self.has_git_interceptor {
            cmd.env("SYSTEM_GIT_PATH", &self.system_git);
            cmd.env("GHERRIT_TEST_DRIVER", &self.gherrit_bin_path);
            // Git prepends its own exec path while invoking hooks. Let the
            // non-shipping test driver restore the hermetic PATH before
            // production code starts any nested Git adapter.
            cmd.env("GHERRIT_TEST_INTERCEPT_PATH", self.test_environment.variable("PATH"));

            if let Some(server) = &self.mock_server {
                cmd.env("GHERRIT_MOCK_SERVER_URL", &server.url);
            }
        }

        // These variables belong on the outer Git command too: installed
        // hooks inherit its environment when Git invokes GHerrit.
        if self.has_mock_github {
            let server = self.mock_server.as_ref().expect("mock GitHub server not available");
            cmd.env("GHERRIT_GITHUB_API_URL", &server.url);
            cmd.env("GITHUB_TOKEN", "mock-token");
        }
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn gherrit_cmd(&self) -> TestCommand {
        // Use injected binary path
        let mut cmd = TestCommand::new(&self.gherrit_bin_path);
        cmd.current_dir(&self.repo_path);

        self.configure_test_env(&mut cmd);

        cmd
    }

    /// Re-executes the current test binary through the bounded, hermetic
    /// command harness for a single-process fixture.
    #[must_use = "command builders do nothing until executed"]
    pub fn reexec_test_cmd(&self) -> TestCommand {
        self.test_environment.command(&env::current_exe().expect("current test executable"))
    }

    /// Executes a production-built command through the bounded, hermetic test
    /// harness while preserving its explicit environment overrides.
    #[must_use = "command builders do nothing until executed"]
    pub fn bounded_cmd(&self, command: Command) -> TestCommand {
        let mut cmd = TestCommand::from_command(command);
        // `Command` records only explicit overrides. Capture them before
        // replacing ambient inheritance, then restore them after fixture
        // capabilities so production `env` and `env_remove` choices win.
        let overrides = cmd.environment_overrides();
        self.test_environment.apply_to_command(&mut cmd);
        self.configure_test_capabilities(&mut cmd);
        cmd.apply_environment_overrides(overrides);
        cmd
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn remote_git_cmd(&self) -> TestCommand {
        assert!(self.has_remote, "missing test capability: .with_remote()");
        let mut cmd = TestCommand::new(&self.system_git);
        cmd.current_dir(&self.remote_path);
        self.configure_test_env(&mut cmd);
        cmd
    }

    pub fn run_git(&self, args: &[&str]) {
        self.git_cmd().args(args).assert().success();
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn git_cmd(&self) -> TestCommand {
        let mut cmd = TestCommand::new("git");
        cmd.current_dir(&self.repo_path);
        self.configure_test_env(&mut cmd);
        cmd
    }

    fn mock_state(&self) -> &Arc<RwLock<mock_server::MockState>> {
        self.mock_server_state
            .as_ref()
            .expect("missing test capability: .with_mock_github() or .with_git_interceptor()")
    }

    pub fn mock_server_url(&self) -> &str {
        assert!(self.has_mock_github, "missing test capability: .with_mock_github()");
        &self.mock_server.as_ref().expect("mock GitHub server not available").url
    }

    pub fn commit(&self, msg: &str) {
        self.run_git(&["commit", "--allow-empty", "-m", msg]);
    }

    /// Creates a commit with a deterministic, syntactically valid GHerrit ID.
    ///
    /// This is fixture setup, not a commit-hook boundary. `--no-verify` makes
    /// that distinction explicit and prevents an installed hook from changing
    /// the supplied identity.
    pub fn commit_with_gherrit_id(&self, message: &str) -> String {
        let sequence = self.next_gherrit_id.fetch_add(1, Ordering::Relaxed);
        let id = deterministic_gherrit_id(sequence);
        self.commit_with_explicit_gherrit_id(message, &id);
        id
    }

    /// Creates a commit with a caller-supplied scenario identity.
    pub fn commit_with_explicit_gherrit_id(&self, message: &str, id: &str) {
        assert!(
            !message.lines().any(|line| line.starts_with("gherrit-pr-id: ")),
            "the explicit GHerrit ID must not also appear in the message"
        );
        assert!(
            !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric()),
            "GHerrit IDs contain only ASCII letters and numbers"
        );

        let message = format!("{message}\n\ngherrit-pr-id: {id}");
        self.run_git(&["commit", "--allow-empty", "--no-verify", "-m", &message]);
    }

    pub fn amend(&self) {
        self.amend_inner(&["--no-edit"]);
    }

    pub fn amend_with_message(&self, message: &str) {
        let previous_id = self.gherrit_id("HEAD").ok();
        let message = if message.lines().any(|line| line.starts_with("gherrit-pr-id: ")) {
            message.to_string()
        } else if let Some(id) = &previous_id {
            format!("{message}\n\ngherrit-pr-id: {id}")
        } else {
            message.to_string()
        };

        self.amend_inner(&["-m", &message]);
    }

    fn amend_inner(&self, args: &[&str]) {
        let previous_oid = self.head_oid();
        let previous_id = self.gherrit_id("HEAD").ok();

        self.git_cmd()
            .arg("commit")
            .arg("--amend")
            .args(args)
            .arg("--allow-empty")
            .assert()
            .success();

        let amended_oid = self.head_oid();
        assert_ne!(previous_oid, amended_oid, "Amend must create a distinct commit object");
        if let Some(previous_id) = previous_id {
            assert_eq!(
                self.gherrit_id("HEAD").unwrap(),
                previous_id,
                "Amend must preserve the GHerrit ID"
            );
        }
    }

    pub fn head_oid(&self) -> String {
        let assert = self.git_cmd().args(["rev-parse", "HEAD"]).assert().success();
        String::from_utf8(assert.get_output().stdout.clone()).unwrap().trim().to_string()
    }

    fn seed_remote_main(&self) {
        self.test_environment
            .command(&self.system_git)
            .current_dir(&self.repo_path)
            .args(["push", "--quiet", "--no-verify", "origin", "refs/heads/main:refs/heads/main"])
            .assert()
            .success();
    }

    pub fn checkout_new(&self, branch_name: &str) {
        self.run_git(&["checkout", "-b", branch_name]);
    }

    pub fn checkout_managed_private(&self, branch_name: &str) {
        self.checkout_new(branch_name);
        self.configure_managed_private(branch_name);
    }

    pub fn checkout_managed_public(&self, branch_name: &str) {
        self.checkout_new(branch_name);
        self.configure_managed_public(branch_name);
    }

    pub fn configure_managed_private(&self, branch_name: &str) {
        self.configure_managed(branch_name, MANAGED_PRIVATE, ".");
    }

    pub fn configure_managed_public(&self, branch_name: &str) {
        self.configure_managed(branch_name, MANAGED_PUBLIC, ".");
    }

    fn configure_managed(&self, branch_name: &str, state: &str, push_remote: &str) {
        let key = |suffix: &str| format!("branch.{branch_name}.{suffix}");
        self.set_config(&key("gherritManaged"), Some(state));
        self.set_config(&key("pushRemote"), Some(push_remote));
        self.set_config(&key("remote"), Some("."));
        self.set_config(&key("merge"), Some(&format!("refs/heads/{branch_name}")));
    }

    pub fn inject_failure(&self, kind: FailureKind) {
        match kind {
            FailureKind::LosePublicationPushReceipt(_) | FailureKind::Git(_) => assert!(
                self.has_git_interceptor,
                "missing test capability: .with_git_interceptor()"
            ),
            _ => assert!(self.has_mock_github, "missing test capability: .with_mock_github()"),
        }
        self.enqueue_failure(kind);
    }

    pub fn expect_git_failure(&self, operation: GitOperation) {
        self.inject_failure(FailureKind::Git(operation));
    }

    /// Schedules one remote ref update after observation and immediately
    /// before GHerrit's next publication push.
    pub fn update_remote_ref_before_push(&self, ref_name: &str, target: &str) {
        assert!(self.has_remote, "missing test capability: .with_remote()");
        assert!(self.has_git_interceptor, "missing test capability: .with_git_interceptor()");
        self.mutate_mock_state(|state| {
            state.git.update_remote_ref_before_push(ref_name.to_owned(), target.to_owned());
        });
    }

    fn enqueue_failure(&self, kind: FailureKind) {
        self.mock_state().write().unwrap().faults.push_back(kind);
    }

    pub fn limit_graphql_query_operations_per_request(&self, limit: usize) {
        assert!(self.has_mock_github, "missing test capability: .with_mock_github()");
        assert_ne!(limit, 0, "GraphQL query operation limit must be nonzero");
        self.mock_state().write().unwrap().max_graphql_query_operations_per_request = Some(limit);
    }

    pub fn assert_failure_consumed(&self) {
        self.inspect_mock_state(|state| {
            assert!(
                state.faults.is_empty(),
                "Expected injected failures to be consumed, but {:?} remain",
                state.faults
            );
        });
    }

    fn inspect_mock_state<T>(&self, f: impl FnOnce(&mock_server::MockState) -> T) -> T {
        let state = self.mock_state().read().unwrap();
        f(&state)
    }

    fn mutate_mock_state<T>(&self, f: impl FnOnce(&mut mock_server::MockState) -> T) -> T {
        let mut state = self.mock_state().write().unwrap();
        f(&mut state)
    }

    pub fn github(&self) -> MockGithub<'_> {
        assert!(self.has_mock_github, "missing test capability: .with_mock_github()");
        MockGithub { context: self }
    }

    pub fn recorded_pushes(&self) -> Vec<PushRecord> {
        assert!(self.has_git_interceptor, "missing test capability: .with_git_interceptor()");
        self.inspect_mock_state(|state| {
            state
                .git
                .pushes()
                .iter()
                .map(|push| PushRecord {
                    arguments: push.arguments().to_vec(),
                    exit_code: push.exit_code(),
                })
                .collect()
        })
    }

    pub fn recorded_git_operations(&self) -> Vec<GitOperation> {
        assert!(self.has_git_interceptor, "missing test capability: .with_git_interceptor()");
        self.inspect_mock_state(|state| state.git.operations().to_vec())
    }

    pub fn publication_overlap(&self) -> &PublicationOverlapController {
        self.publication_overlap
            .as_ref()
            .expect("missing test capability: .with_publication_overlap()")
    }

    pub fn formatted_github_state(&self) -> String {
        let state = self.github().pull_requests();
        let json = serde_json::to_string_pretty(&state).expect("Failed to serialize PRs");
        self.sanitize(&json)
    }

    pub fn remote_ref_oid(&self, ref_name: &str) -> Option<String> {
        let output = self
            .remote_git_cmd()
            .args(["rev-parse", "--verify", "--quiet", ref_name])
            .output()
            .expect("Failed to inspect remote ref");
        match output.status.code() {
            Some(0) => Some(String::from_utf8(output.stdout).unwrap().trim().to_string()),
            Some(1) => None,
            code => panic!("git rev-parse failed with exit code {code:?}"),
        }
    }

    pub fn remote_refs(&self, prefix: &str) -> Vec<String> {
        let assert = self
            .remote_git_cmd()
            .args(["for-each-ref", "--format=%(refname)", prefix])
            .assert()
            .success();
        String::from_utf8(assert.get_output().stdout.clone())
            .unwrap()
            .lines()
            .map(ToString::to_string)
            .collect()
    }

    pub fn init_bare_repo(&self, path: &Path) {
        init_git_bare_repo(&self.test_environment, &self.system_git, path);
    }

    pub fn set_config(&self, key: &str, value: Option<&str>) {
        if let Some(val) = value {
            self.git_cmd().args(["config", key, val]).assert().success();
        } else {
            let output = self
                .git_cmd()
                .args(["config", "--get-all", key])
                .output()
                .expect("Failed to inspect Git config");
            match output.status.code() {
                Some(0) => {
                    self.git_cmd().args(["config", "--unset-all", key]).assert().success();
                }
                Some(1) => {}
                code => panic!(
                    "Failed to inspect Git config {key:?} with exit code {code:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            }
        }
    }

    pub fn assert_config(&self, key: &str, expected_value: Option<&str>) {
        if let Some(val) = expected_value {
            self.git_cmd().args(["config", key]).assert().success().stdout(format!("{}\n", val));
        } else {
            self.git_cmd().args(["config", key]).assert().code(1).stdout("");
        }
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn hook_cmd(&self, name: &str) -> TestCommand {
        let mut cmd = self.gherrit_cmd();
        cmd.args(["hook", name]);
        cmd
    }

    /// Runs one hook script installed in this fixture with the same hermetic
    /// environment inherited from a fixture Git process.
    #[must_use = "command builders do nothing until executed"]
    pub fn installed_hook_cmd(&self, name: &str) -> TestCommand {
        let hook = self.repo_path.join(".git/hooks").join(name);
        assert!(hook.is_file(), "hook {name:?} is not installed");
        let mut cmd = TestCommand::new(hook);
        cmd.current_dir(&self.repo_path);
        self.configure_test_env(&mut cmd);
        cmd
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn manage_cmd(&self) -> TestCommand {
        let mut cmd = self.gherrit_cmd();
        cmd.arg("manage");
        cmd
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn unmanage_cmd(&self) -> TestCommand {
        let mut cmd = self.gherrit_cmd();
        cmd.arg("unmanage");
        cmd
    }

    pub fn gherrit_id(&self, ref_name: &str) -> Result<String, Box<dyn std::error::Error>> {
        let assert = self.git_cmd().args(["log", "-1", "--format=%B", ref_name]).assert().success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone())?;

        stdout
            .lines()
            .find_map(|line| line.strip_prefix("gherrit-pr-id: "))
            .map(|id| id.trim().to_string())
            .ok_or_else(|| format!("Commit {} is missing 'gherrit-pr-id' trailer", ref_name).into())
    }

    pub fn sanitize(&self, output: &str) -> String {
        self.sanitize_with_redactions(output, &[])
    }

    pub fn sanitize_with_redactions(&self, output: &str, redactions: &[(&str, &str)]) -> String {
        let repo_path = self.repo_path.to_str().unwrap();
        let remote_path = self.remote_path.to_str().unwrap();
        let redactions = expand_literal_redactions(
            redactions
                .iter()
                .copied()
                .chain([(repo_path, "[REPO_PATH]"), (remote_path, "[REMOTE_PATH]")]),
        );

        let output = apply_literal_redactions(
            output,
            redactions.iter().map(|(target, replacement)| (target.as_str(), replacement.as_str())),
        );
        static SHA_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\b[0-9a-f]{40}\b").expect("Invalid regex"));

        static MOCK_URL_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"http://127\.0\.0\.1:\d+").expect("Invalid regex"));

        static GHERRIT_ID_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\bG[a-zA-Z0-9]{16,}\b").expect("Invalid regex"));

        let output = redact_identities(&output, &SHA_REGEX, "SHA");
        let output = redact_identities(&output, &GHERRIT_ID_REGEX, "GHERRIT_ID");
        MOCK_URL_REGEX.replace_all(&output, "[MOCK_SERVER_URL]").to_string()
    }
}

fn normalize_command_output(output: &str) -> String {
    // This Git error message only appears on some platforms/versions.
    let output = output.replace("fatal: the remote end hung up unexpectedly\n", "");
    normalize_git_diagnostic_separators(&output)
}

#[cfg(any(test, windows))]
fn normalize_windows_stderr(stderr: &str) -> String {
    static CLAP_USAGE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^Usage: gherrit\.exe(?P<suffix>[ \t\r]|$)").expect("Invalid regex")
    });
    static COMMAND_STATUS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?m)^(?P<prefix>\[gherrit\](?: \[[A-Z]+\])? [^\r\n]*\bCommand(?: [^\r\n]*)? failed with status: )exit code: ",
        )
        .expect("Invalid regex")
    });
    static MISSING_TMP_WARNING_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?m)^(?P<prefix>\[gherrit\](?: \[[A-Z]+\])? [^\r\n]*\bStderr: )?bash\.exe: warning: could not find (?:/tmp,|<path or URL redacted>) please create!(?:\r?\n|$)",
        )
        .expect("Invalid regex")
    });

    let stderr = MISSING_TMP_WARNING_REGEX.replace_all(stderr, "${prefix}");
    let stderr = CLAP_USAGE_REGEX.replace_all(&stderr, "Usage: gherrit${suffix}");
    COMMAND_STATUS_REGEX.replace_all(&stderr, "${prefix}exit status: ").into_owned()
}

#[cfg(any(test, windows))]
fn normalize_windows_redacted_paths(output: &str) -> String {
    static REDACTED_PATH_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\[(?:REPO_PATH|REMOTE_PATH|EXTERNAL_HOOKS_PATH)\](?:\\+[^\\\s"']+)*"#)
            .expect("Invalid regex")
    });
    static WINDOWS_SEPARATOR_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\\+").expect("Invalid regex"));

    REDACTED_PATH_REGEX
        .replace_all(output, |path: &regex::Captures<'_>| {
            WINDOWS_SEPARATOR_REGEX.replace_all(&path[0], "/").into_owned()
        })
        .into_owned()
}

fn normalize_git_diagnostic_separators(output: &str) -> String {
    static ATOMIC_PUSH_SEPARATOR_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^(error: atomic push failed[^\r\n]*\r?\n)(?:\r?\n)+(To )")
            .expect("Invalid regex")
    });

    ATOMIC_PUSH_SEPARATOR_REGEX.replace_all(output, "${1}${2}").into_owned()
}

pub trait IntoCommandRef {
    fn as_command_mut(&mut self) -> &mut TestCommand;
}

impl IntoCommandRef for TestCommand {
    fn as_command_mut(&mut self) -> &mut TestCommand {
        self
    }
}

impl IntoCommandRef for &mut TestCommand {
    fn as_command_mut(&mut self) -> &mut TestCommand {
        self
    }
}

impl TestContext {
    pub fn execute_and_format(
        &self,
        mut cmd: impl IntoCommandRef,
        redactions: &[(&str, &str)],
        expected_exit: ExpectedExit,
    ) -> String {
        let cmd = cmd.as_command_mut();
        let output = cmd.output().expect("Failed to execute command");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = normalize_command_output(&stderr);
        #[cfg(windows)]
        let stderr = normalize_windows_stderr(&stderr);
        let succeeded = output.status.success();
        let exit_code = output.status.code().unwrap_or(-1);

        // This output will be stored verbatim in the filesystem.
        let output = format!(
            "EXIT_CODE: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}\n",
            exit_code,
            if stdout.is_empty() { "(empty)" } else { &stdout },
            if stderr.is_empty() { "(empty)" } else { &stderr }
        );
        let output = self.sanitize_with_redactions(&output, redactions);
        #[cfg(windows)]
        let output = normalize_windows_redacted_paths(&output);
        match expected_exit {
            ExpectedExit::Success => {
                assert!(succeeded, "Expected command to succeed:\n{output}");
            }
            ExpectedExit::Failure => {
                assert!(!succeeded, "Expected command to fail:\n{output}");
            }
        }
        output
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedExit {
    Success,
    Failure,
}

fn redact_identities(output: &str, regex: &Regex, namespace: &str) -> String {
    let mut identities = HashMap::new();
    regex
        .replace_all(output, |captures: &regex::Captures<'_>| {
            let next_index = identities.len() + 1;
            let index = identities.entry(captures[0].to_string()).or_insert(next_index);
            format!("[{namespace}_{index}]")
        })
        .into_owned()
}

fn apply_literal_redactions<'a>(
    output: &str,
    redactions: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    redactions.into_iter().fold(output.to_string(), |output, (target, replacement)| {
        output.replace(target, replacement)
    })
}

fn expand_literal_redactions<'a>(
    redactions: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<(String, String)> {
    let mut redactions = redactions
        .into_iter()
        .flat_map(|(target, replacement)| {
            let mut targets = vec![target.to_string()];
            let debug = format!("{target:?}");
            let debug =
                debug.strip_prefix('"').and_then(|debug| debug.strip_suffix('"')).unwrap_or(&debug);
            targets.push(debug.to_string());
            if let Ok(canonical) = fs::canonicalize(target) {
                targets.push(canonical.to_string_lossy().into_owned());

                let debug = format!("{canonical:?}");
                let debug = debug
                    .strip_prefix('"')
                    .and_then(|debug| debug.strip_suffix('"'))
                    .unwrap_or(&debug);
                targets.push(debug.to_string());
            }
            let forward_slash_targets = targets
                .iter()
                .filter(|target| target.contains('\\'))
                .map(|target| target.replace('\\', "/"))
                .collect::<Vec<_>>();
            targets.extend(forward_slash_targets);
            targets.sort();
            targets.dedup();
            targets.sort_by_key(|target| std::cmp::Reverse(target.len()));
            targets.into_iter().map(move |target| (target, replacement.to_string()))
        })
        .collect::<Vec<_>>();
    redactions.sort_by_key(|(target, _)| std::cmp::Reverse(target.len()));
    redactions
}

#[macro_export]
macro_rules! assert_success_snapshot {
    ($ctx:expr, $cmd:expr, $name:expr $(,)?) => {
        $crate::assert_success_snapshot!($ctx, $cmd, $name, &[])
    };
    ($ctx:expr, $cmd:expr, $name:expr, $redactions:expr $(,)?) => {
        let content = $ctx.execute_and_format($cmd, $redactions, $crate::ExpectedExit::Success);
        insta::assert_snapshot!($name, content);
    };
}

#[macro_export]
macro_rules! assert_failure_snapshot {
    ($ctx:expr, $cmd:expr, $name:expr $(,)?) => {
        $crate::assert_failure_snapshot!($ctx, $cmd, $name, &[])
    };
    ($ctx:expr, $cmd:expr, $name:expr, $redactions:expr $(,)?) => {
        let content = $ctx.execute_and_format($cmd, $redactions, $crate::ExpectedExit::Failure);
        insta::assert_snapshot!($name, content);
    };
}

#[macro_export]
macro_rules! assert_pr_snapshot {
    ($ctx:expr, $name:expr $(,)?) => {
        insta::assert_snapshot!($name, $ctx.formatted_github_state());
    };
}

fn deterministic_gherrit_id(mut sequence: u64) -> String {
    const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut encoded = [BASE32[0]; 32];
    encoded.iter_mut().rev().for_each(|digit| {
        *digit = BASE32[(sequence % BASE32.len() as u64) as usize];
        sequence /= BASE32.len() as u64;
    });
    assert_eq!(sequence, 0, "GHerrit ID sequence exceeds its 32-digit encoding");

    format!("G{}", core::str::from_utf8(&encoded).expect("base32 alphabet is UTF-8"))
}

fn run_git_cmd(environment: &TestEnvironment, system_git: &Path, path: &Path, args: &[&str]) {
    environment.command(system_git).current_dir(path).args(args).assert().success();
}

#[cfg(unix)]
fn install_git_interceptor(path: &Path, _gherrit_bin: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let git = path.join("git");
    fs::write(&git, "#!/bin/sh\nexec \"$GHERRIT_TEST_DRIVER\" __test-git \"$@\"\n").unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(windows)]
fn install_git_interceptor(path: &Path, gherrit_bin: &Path) {
    // `Command::new("git")` resolves native executables without consulting
    // PATHEXT, so a batch-file wrapper named `git.cmd` would be bypassed.
    hard_link_or_copy(gherrit_bin, &path.join("git.exe"));
}

fn install_gherrit_binary(path: &Path, gherrit_bin: &Path) {
    let gherrit_dst = path.join(if cfg!(windows) { "gherrit.exe" } else { "gherrit" });
    hard_link_or_copy(gherrit_bin, &gherrit_dst);
}

/// Installs a test executable without copying a large Cargo artifact when the
/// fixture and target directory share a filesystem.
fn hard_link_or_copy(source: &Path, destination: &Path) {
    if let Err(link_error) = fs::hard_link(source, destination) {
        fs::copy(source, destination).unwrap_or_else(|copy_error| {
            panic!(
                "failed to install test executable {:?}: hard link failed ({link_error}); \
                 copy fallback failed ({copy_error})",
                destination
            )
        });
    }
}

fn init_git_bare_repo(environment: &TestEnvironment, system_git: &Path, path: &Path) {
    fs::create_dir(path).unwrap();
    run_git_cmd(environment, system_git, path, &["init", "--bare"]);
    run_git_cmd(environment, system_git, path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
}

fn init_git_repo(
    environment: &TestEnvironment,
    system_git: &Path,
    path: &Path,
    remote_path: Option<&Path>,
) {
    let run = |args| run_git_cmd(environment, system_git, path, args);
    run(&["init"]);
    run(&["config", "core.hooksPath", ".git/hooks"]);
    // Pin both the actual branch and the configuration consulted by default-
    // branch discovery. Ambient Git defaults must not choose fixture topology.
    run(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    run(&["config", "init.defaultBranch", "main"]);
    // Explicitly unmanage main to satisfy strict config checks
    run(&["config", "branch.main.gherritManaged", "false"]);
    if let Some(remote_path) = remote_path {
        let remote_url = remote_path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
        run(&["remote", "add", "origin", &remote_url]);
    }
}

static SYSTEM_GIT: LazyLock<PathBuf> = LazyLock::new(|| -> PathBuf {
    let path = env::var_os("PATH").expect("PATH is required to find system Git");
    find_system_git(&path, env::var_os("PATHEXT").as_deref())
        .expect("Failed to find an executable system Git on PATH")
});

fn find_system_git(
    path: &std::ffi::OsStr,
    path_extensions: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let names = system_git_names(path_extensions);
    env::split_paths(path).find_map(|directory| {
        names.iter().find_map(|name| {
            let candidate = directory.join(name);
            if is_executable_file(&candidate) {
                fs::canonicalize(candidate).ok()
            } else {
                None
            }
        })
    })
}

fn system_git_names(_path_extensions: Option<&std::ffi::OsStr>) -> Vec<OsString> {
    #[cfg(not(windows))]
    {
        vec![OsString::from("git")]
    }
    #[cfg(windows)]
    {
        let extensions =
            _path_extensions.and_then(std::ffi::OsStr::to_str).unwrap_or(".COM;.EXE;.BAT;.CMD");
        let mut names = vec![OsString::from("git.exe")];
        for extension in extensions.split(';').filter(|extension| !extension.is_empty()) {
            names.push(format!("git{extension}").into());
        }
        names
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn poisoned_mock_state_reports_unconsumed_faults() {
        let driver_dir = TempDir::new().unwrap();
        let driver = driver_dir.path().join("gherrit-test-driver");
        fs::write(&driver, "test driver placeholder").unwrap();

        let ctx = TestContextBuilder::new(&driver).with_git_interceptor().build();
        ctx.expect_git_failure(GitOperation::Var);
        let state = ctx.mock_server_state.as_ref().unwrap().clone();
        let poison = panic::catch_unwind(move || {
            let _state = state.write().unwrap();
            panic!("simulated request-handler panic");
        });
        assert!(poison.is_err());

        let panic = panic::catch_unwind(panic::AssertUnwindSafe(|| drop(ctx)))
            .expect_err("dropping the fixture must reject poisoned mock state");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("fixture panic must have a string message");
        assert!(message.contains("was poisoned"), "unexpected teardown panic: {message}");
        assert!(message.contains("Git(Var)"), "unexpected teardown panic: {message}");
    }

    #[test]
    fn deterministic_ids_use_the_production_alphabet_and_width() {
        assert_eq!(deterministic_gherrit_id(0), format!("G{}", "a".repeat(32)));
        assert_eq!(deterministic_gherrit_id(1), format!("G{}b", "a".repeat(31)));
        assert_eq!(deterministic_gherrit_id(31), format!("G{}7", "a".repeat(31)));
        assert_eq!(deterministic_gherrit_id(32), format!("G{}ba", "a".repeat(30)));
    }

    #[test]
    fn ordinary_pull_request_seeds_match_the_graphql_number_domain() {
        let seed = |number| PullRequestSeed::new(number, "title", "body", "head", "base");

        assert_eq!(seed(1).number.get(), 1);
        assert!(!seed(1).is_draft);
        assert!(seed(1).draft().is_draft);
        assert_eq!(seed(i32::MAX as usize).number.get(), i32::MAX as u32);
        for invalid in [0, i32::MAX as usize + 1] {
            assert!(
                panic::catch_unwind(|| seed(invalid)).is_err(),
                "accepted invalid pull request number {invalid}"
            );
        }
    }

    #[test]
    fn explicit_management_helpers_write_complete_branch_state() {
        let ctx = TestContextBuilder::new("unused").with_initial_commit().build();
        let assert_state = |branch: &str, state: &str, push_remote: &str| {
            ctx.assert_config(&format!("branch.{branch}.gherritManaged"), Some(state));
            ctx.assert_config(&format!("branch.{branch}.pushRemote"), Some(push_remote));
            ctx.assert_config(&format!("branch.{branch}.remote"), Some("."));
            ctx.assert_config(
                &format!("branch.{branch}.merge"),
                Some(&format!("refs/heads/{branch}")),
            );
        };

        ctx.checkout_managed_private("private-stack");
        assert_state("private-stack", MANAGED_PRIVATE, ".");

        ctx.run_git(&["checkout", "main"]);
        ctx.checkout_managed_public("public-stack");
        assert_state("public-stack", MANAGED_PUBLIC, ".");
    }

    #[test]
    fn interceptor_only_context_rejects_unconsumed_git_faults() {
        let driver_dir = TempDir::new().unwrap();
        let driver = driver_dir.path().join("gherrit-test-driver");
        fs::write(&driver, "test driver placeholder").unwrap();

        let panic = panic::catch_unwind(|| {
            let ctx = TestContextBuilder::new(&driver).with_git_interceptor().build();
            ctx.expect_git_failure(GitOperation::Var);
        })
        .expect_err("dropping the fixture must reject an unconsumed Git fault");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("fixture panic must have a string message");
        assert!(message.contains("Git(Var)"), "unexpected teardown panic: {message}");
    }

    #[test]
    fn fixture_rejects_an_unconsumed_scheduled_remote_ref_update() {
        let driver_dir = TempDir::new().unwrap();
        let driver = driver_dir.path().join("gherrit-test-driver");
        fs::write(&driver, "test driver placeholder").unwrap();

        let panic = panic::catch_unwind(|| {
            let ctx = TestContextBuilder::new(&driver).with_remote().with_git_interceptor().build();
            ctx.update_remote_ref_before_push("refs/tags/race", &"1".repeat(40));
        })
        .expect_err("dropping the fixture must reject an unconsumed scheduled update");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("fixture panic must have a string message");
        assert!(
            message.contains("unconsumed scheduled remote ref updates: 1"),
            "unexpected teardown panic: {message}"
        );
    }

    #[test]
    fn identity_redaction_preserves_equality_and_order() {
        let regex = Regex::new(r"G[a-z0-9]+").unwrap();
        let output = redact_identities("Galpha Gbeta Galpha Ggamma Gbeta", &regex, "ID");

        assert_eq!(output, "[ID_1] [ID_2] [ID_1] [ID_3] [ID_2]");
    }

    #[test]
    fn identity_namespaces_are_independent() {
        let sha_regex = Regex::new(r"[0-9a-f]{40}").unwrap();
        let id_regex = Regex::new(r"G[a-z0-9]+").unwrap();
        let first_sha = "1111111111111111111111111111111111111111";
        let second_sha = "2222222222222222222222222222222222222222";
        let output = format!("{first_sha} Gone {second_sha} Gtwo {first_sha} Gtwo");
        let output = redact_identities(&output, &sha_regex, "SHA");
        let output = redact_identities(&output, &id_regex, "GHERRIT_ID");

        assert_eq!(output, "[SHA_1] [GHERRIT_ID_1] [SHA_2] [GHERRIT_ID_2] [SHA_1] [GHERRIT_ID_2]");
    }

    #[test]
    fn literal_redactions_take_precedence_over_identity_redaction() {
        let regex = Regex::new(r"[0-9a-f]{40}").unwrap();
        let base_sha = "1111111111111111111111111111111111111111";
        let other_sha = "2222222222222222222222222222222222222222";
        let output = format!("{base_sha} {other_sha} {base_sha}");
        let output = apply_literal_redactions(&output, [(base_sha, "[BASE_SHA]")]);
        let output = redact_identities(&output, &regex, "SHA");

        assert_eq!(output, "[BASE_SHA] [SHA_1] [BASE_SHA]");
    }

    #[test]
    fn normalizes_windows_stderr_without_rewriting_payloads() {
        let stderr = concat!(
            "Usage: gherrit.exe manage\r\n",
            "[gherrit] [WARN] Failed: Command \"git\" failed with status: exit code: 128\r\n",
            "bash.exe: warning: could not find /tmp, please create!\r\n",
            "bash.exe: warning: could not find <path or URL redacted> please create!\r\n",
            "[gherrit] [WARN] Nested command failed. Stderr: bash.exe: warning: could not find /tmp, please create!\r\n",
            "fatal: repository missing\r\n",
            "[gherrit] [WARN] Redacted command failed. Stderr: bash.exe: warning: could not find <path or URL redacted> please create!\r\n",
            "fatal: redacted repository missing\r\n",
            "payload: Usage: gherrit.exe manage\r\n",
            "payload: Command failed with status: exit code: 2\r\n",
            "payload: bash.exe: warning: could not find /tmp, please create!\r\n",
            "payload Stderr: bash.exe: warning: could not find /tmp, please create!\r\n",
            "payload: bash.exe: warning: could not find <path or URL redacted> please create!\r\n",
            "payload Stderr: bash.exe: warning: could not find <path or URL redacted> please create!\r\n",
            r#"payload: {"title":"Usage: gherrit.exe","body":"line\n{\"key\":\"value\"}"}"#,
        );

        assert_eq!(
            normalize_windows_stderr(stderr),
            concat!(
                "Usage: gherrit manage\r\n",
                "[gherrit] [WARN] Failed: Command \"git\" failed with status: exit status: 128\r\n",
                "[gherrit] [WARN] Nested command failed. Stderr: fatal: repository missing\r\n",
                "[gherrit] [WARN] Redacted command failed. Stderr: fatal: redacted repository missing\r\n",
                "payload: Usage: gherrit.exe manage\r\n",
                "payload: Command failed with status: exit code: 2\r\n",
                "payload: bash.exe: warning: could not find /tmp, please create!\r\n",
                "payload Stderr: bash.exe: warning: could not find /tmp, please create!\r\n",
                "payload: bash.exe: warning: could not find <path or URL redacted> please create!\r\n",
                "payload Stderr: bash.exe: warning: could not find <path or URL redacted> please create!\r\n",
                r#"payload: {"title":"Usage: gherrit.exe","body":"line\n{\"key\":\"value\"}"}"#,
            )
        );
    }

    #[test]
    fn normalizes_only_redacted_windows_paths() {
        let output = concat!(
            "[REPO_PATH]\\.git\\hooks\\pre-push\n",
            "[REMOTE_PATH]\\\\objects\\\\pack\n",
            r#"{"body":"line\n{\"path\":\"C:\\unredacted\"}"}"#,
        );

        assert_eq!(
            normalize_windows_redacted_paths(output),
            concat!(
                "[REPO_PATH]/.git/hooks/pre-push\n",
                "[REMOTE_PATH]/objects/pack\n",
                r#"{"body":"line\n{\"path\":\"C:\\unredacted\"}"}"#,
            )
        );
    }

    #[test]
    fn expands_path_redaction_spellings() {
        let root = TempDir::new().unwrap();
        let path = root.path().to_str().unwrap();
        let canonical = fs::canonicalize(path).unwrap();
        let debug = format!("{canonical:?}");
        let debug = debug.trim_matches('"');
        let redactions = expand_literal_redactions([(path, "[ROOT]")]);

        let redact = |output| {
            apply_literal_redactions(
                output,
                redactions
                    .iter()
                    .map(|(target, replacement)| (target.as_str(), replacement.as_str())),
            )
        };
        assert_eq!(redact(canonical.to_str().unwrap()), "[ROOT]");
        assert_eq!(redact(debug), "[ROOT]");

        let windows_path = r"C:\root";
        let debug = format!("{windows_path:?}");
        let debug = debug.trim_matches('"');
        let redactions = expand_literal_redactions([(windows_path, "[ROOT]")]);
        let redact = |output| {
            apply_literal_redactions(
                output,
                redactions
                    .iter()
                    .map(|(target, replacement)| (target.as_str(), replacement.as_str())),
            )
        };
        assert_eq!(redact(debug), "[ROOT]");
        assert_eq!(redact("C:/root/child"), "[ROOT]/child");
    }

    #[test]
    fn applies_longest_literal_redactions_first() {
        let root = TempDir::new().unwrap();
        let child = root.path().join("child");
        fs::create_dir(&child).unwrap();
        let root = root.path().to_str().unwrap();
        let child = child.to_str().unwrap();
        let redactions = expand_literal_redactions([(root, "[ROOT]"), (child, "[CHILD]")]);

        assert_eq!(
            apply_literal_redactions(
                child,
                redactions
                    .iter()
                    .map(|(target, replacement)| (target.as_str(), replacement.as_str())),
            ),
            "[CHILD]"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_environment_clears_inherited_values() {
        let root = TempDir::new().unwrap();
        let environment = TestEnvironment::new(root.path(), SYSTEM_GIT.as_path());
        let mut command = TestCommand::new("/usr/bin/env");
        command.env("SHOULD_BE_CLEARED", "yes");
        environment.apply_to_command(&mut command);

        let assert = command.assert().success();
        let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert!(!output.contains("SHOULD_BE_CLEARED="));
        assert!(output.lines().any(|line| line == "RUST_LOG=info"));
        assert!(output.lines().any(|line| line.starts_with("HOME=")));
    }

    #[test]
    #[cfg(unix)]
    fn bounded_command_preserves_overrides_after_context_capabilities() {
        let context = TestContextBuilder::new("unused").build();
        let mut raw = Command::new("/usr/bin/env");
        raw.env("EXPLICIT_FIXTURE_VALUE", "present")
            .env("GIT_AUTHOR_DATE", "explicit-author-date")
            .env_remove("GIT_COMMITTER_DATE")
            .env_remove("RUST_LOG");
        let mut command = context.bounded_cmd(raw);

        let output =
            String::from_utf8(command.assert().success().get_output().stdout.clone()).unwrap();
        assert!(output.lines().any(|line| line == "EXPLICIT_FIXTURE_VALUE=present"));
        assert!(output.lines().any(|line| line == "GIT_AUTHOR_DATE=explicit-author-date"));
        assert!(!output.lines().any(|line| line.starts_with("GIT_COMMITTER_DATE=")));
        assert!(!output.lines().any(|line| line.starts_with("RUST_LOG=")));
        assert!(output.lines().any(|line| line.starts_with("HOME=")));
    }

    #[test]
    fn system_git_resolution_uses_path_order_and_executable_files() {
        let root = TempDir::new().unwrap();
        let skipped = root.path().join("skipped");
        let selected = root.path().join("selected");
        fs::create_dir(&skipped).unwrap();
        fs::create_dir(&selected).unwrap();
        #[cfg(windows)]
        let name = "git.EXE";
        #[cfg(not(windows))]
        let name = "git";
        let executable = selected.join(name);
        fs::write(&executable, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let non_executable = skipped.join(name);
            fs::write(&non_executable, b"fixture").unwrap();
        }
        let path = env::join_paths([&skipped, &selected]).unwrap();

        let resolved = find_system_git(&path, Some(std::ffi::OsStr::new(".EXE"))).unwrap();

        assert_eq!(resolved, fs::canonicalize(executable).unwrap());
    }

    #[test]
    fn test_environment_provides_a_deterministic_git_identity() {
        let root = TempDir::new().unwrap();
        let environment = TestEnvironment::new(root.path(), SYSTEM_GIT.as_path());
        let repository = root.path().join("secondary");
        fs::create_dir(&repository).unwrap();
        environment
            .command(SYSTEM_GIT.as_path())
            .current_dir(&repository)
            .args(["init", "--quiet"])
            .assert()
            .success();
        environment
            .command(SYSTEM_GIT.as_path())
            .current_dir(&repository)
            .args(["commit", "--allow-empty", "--message", "Test identity"])
            .env("GIT_AUTHOR_DATE", "@946684800 +0000")
            .env("GIT_COMMITTER_DATE", "@946684800 +0000")
            .assert()
            .success();

        let output = environment
            .command(SYSTEM_GIT.as_path())
            .current_dir(&repository)
            .args(["show", "--no-patch", "--format=%an%n%ae%n%cn%n%ce", "HEAD"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Test User\ntest@example.com\nTest User\ntest@example.com\n"
        );
    }

    #[test]
    fn server_completion_wait_is_bounded() {
        let (_completion_tx, completion_rx) = mpsc::channel();
        let timeout = Duration::from_millis(20);
        let started = Instant::now();

        let error = wait_for_server_completion(&completion_rx, timeout).unwrap_err();

        assert!(matches!(error, MockServerStopError::TimedOut(value) if value == timeout));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn server_completion_propagates_panics() {
        let (completion_tx, completion_rx): (_, Receiver<thread::Result<()>>) = mpsc::channel();
        assert!(completion_tx.send(Err(Box::new("server panic"))).is_ok());

        let error = wait_for_server_completion(&completion_rx, Duration::from_secs(1)).unwrap_err();

        let MockServerStopError::Panicked(payload) = error else {
            panic!("expected server panic result");
        };
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"server panic"));
    }

    #[test]
    fn dropping_mock_server_waits_for_state_release() {
        let state = Arc::new(RwLock::new(mock_server::MockState::default()));
        let weak_state = Arc::downgrade(&state);
        let test_dir = Arc::new(TempDir::new().unwrap());
        let system_git = SYSTEM_GIT.clone();
        let test_environment = TestEnvironment::new(test_dir.path(), &system_git);
        let server = MockServerInfo::start(
            state,
            test_dir.path().join("remote.git"),
            system_git,
            test_environment,
            test_dir,
            None,
        );

        assert!(weak_state.upgrade().is_some());
        drop(server);
        assert!(weak_state.upgrade().is_none(), "mock server retained state after teardown");
    }

    #[test]
    fn normalizes_only_atomic_push_separator_lines() {
        let canonical = concat!(
            "before\n",
            "error: atomic push failed for ref refs/tags/v2. status: 7\n",
            "To remote\n",
            "after\n",
        );
        let with_separator = concat!(
            "before\n",
            "error: atomic push failed for ref refs/tags/v2. status: 7\n",
            "\n",
            "To remote\n",
            "after\n",
        );
        assert_eq!(normalize_git_diagnostic_separators(canonical), canonical);
        assert_eq!(normalize_git_diagnostic_separators(with_separator), canonical);

        let unrelated = "error: another failure\n\nTo remote\n";
        assert_eq!(normalize_git_diagnostic_separators(unrelated), unrelated);
    }
}
