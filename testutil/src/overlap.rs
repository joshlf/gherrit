//! Overlap.

use super::*;

pub(super) struct ObservationArrival {
    pub(super) matches: AtomicUsize,
    pub(super) notify: tokio::sync::Notify,
}

impl ObservationArrival {
    pub(super) fn new() -> Self {
        Self { matches: AtomicUsize::new(0), notify: tokio::sync::Notify::new() }
    }
}

/// A bounded rendezvous for the two observations in one publication attempt.
///
/// The first exact Git request and first local-ID GitHub request wait here
/// before entering mutable fake state. A concurrent implementation releases
/// both immediately. Sequential awaiting fails within a fixed deadline, and
/// server shutdown can cancel either handler without a controller thread.
pub struct ObservationOverlapSchedule {
    pub(super) exact_git: ObservationArrival,
    pub(super) local_github: ObservationArrival,
    pub(super) cancelled: AtomicBool,
    pub(super) deadline: OnceLock<tokio::time::Instant>,
    pub(super) expected_id: Box<str>,
    pub(super) expected_git_patterns: Box<[String]>,
}

impl ObservationOverlapSchedule {
    pub(super) fn new(id: &str, default_branch: &str) -> Arc<Self> {
        let tag_root = format!("refs/tags/gherrit/{id}");
        Arc::new(Self {
            exact_git: ObservationArrival::new(),
            local_github: ObservationArrival::new(),
            cancelled: AtomicBool::new(false),
            deadline: OnceLock::new(),
            expected_id: id.into(),
            expected_git_patterns: [
                format!("refs/heads/{default_branch}"),
                format!("refs/heads/{id}"),
                format!("refs/heads/gherrit-bases/{id}"),
                tag_root.clone(),
                format!("{tag_root}/*"),
            ]
            .into(),
        })
    }

    async fn arrive(&self, own: &ObservationArrival, peer: &ObservationArrival) -> bool {
        if own.matches.fetch_add(1, Ordering::AcqRel) != 0 {
            self.cancel();
            return false;
        }
        own.notify.notify_one();
        let deadline = *self
            .deadline
            .get_or_init(|| tokio::time::Instant::now() + OBSERVATION_OVERLAP_TIMEOUT);
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return false;
            }
            if peer.matches.load(Ordering::Acquire) != 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, peer.notify.notified()).await.is_err() {
                self.cancel();
                return false;
            }
        }
    }

    pub(crate) async fn exact_git_arrived(&self) -> bool {
        self.arrive(&self.exact_git, &self.local_github).await
    }

    pub(crate) async fn local_github_arrived(&self) -> bool {
        self.arrive(&self.local_github, &self.exact_git).await
    }

    pub(crate) fn expected_id(&self) -> &str {
        &self.expected_id
    }

    pub(crate) fn expected_git_patterns(&self) -> &[String] {
        &self.expected_git_patterns
    }

    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.exact_git.notify.notify_one();
        self.local_github.notify.notify_one();
    }

    pub fn assert_consumed(&self) {
        assert!(!self.cancelled.load(Ordering::Acquire), "observation schedule was cancelled");
        assert_eq!(self.exact_git.matches.load(Ordering::Acquire), 1, "exact Git request count");
        assert_eq!(
            self.local_github.matches.load(Ordering::Acquire),
            1,
            "local GitHub request count"
        );
    }
}

/// Server-side gates for one deliberately scheduled two-publisher overlap.
///
/// Semaphores retain releases even if teardown wins a race with a handler.
/// Arrival channels let the synchronous test controller inspect durable state
/// without sleeping or entering the server's state lock.
pub(crate) struct PublicationOverlapSchedule {
    pub(super) cancelled: AtomicBool,
    pub(super) create_slots: AtomicUsize,
    pub(super) create_arrived: mpsc::Sender<()>,
    pub(super) create_application_release: Arc<tokio::sync::Semaphore>,
    pub(super) create_applied: mpsc::Sender<()>,
    pub(super) create_response_release: Arc<tokio::sync::Semaphore>,
    pub(super) marker_arrived: mpsc::Sender<()>,
    pub(super) marker_release: Arc<tokio::sync::Semaphore>,
    pub(super) marker_slots: AtomicUsize,
}

impl PublicationOverlapSchedule {
    pub(super) fn new() -> (Arc<Self>, PublicationOverlapController) {
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

    pub(super) fn claim(slot_count: &AtomicUsize) -> bool {
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

    pub(super) fn release_all(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.create_application_release.add_permits(2);
        self.create_response_release.add_permits(2);
        self.marker_release.add_permits(2);
    }
}

/// Synchronous control surface for the deterministic overlap schedule.
pub struct PublicationOverlapController {
    pub(super) schedule: Arc<PublicationOverlapSchedule>,
    pub(super) create_arrivals: Receiver<()>,
    pub(super) create_applications: Receiver<()>,
    pub(super) marker_arrivals: Receiver<()>,
    pub(super) deadline: Cell<Option<Instant>>,
}

impl PublicationOverlapController {
    pub(super) fn deadline(&self) -> Instant {
        self.deadline.get().expect("an overlap cancellation guard must be active")
    }

    pub(super) fn wait_twice(&self, receiver: &Receiver<()>, boundary: &str) {
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
    pub(super) schedule: &'schedule PublicationOverlapSchedule,
    pub(super) armed: bool,
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
