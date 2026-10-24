use super::*;
use std::collections::VecDeque;
pub(super) fn watched(id: &str) -> WatchedContainer {
    WatchedContainer {
        id: id.to_owned(),
        created: "2026-01-01T00:00:00Z".to_owned(),
    }
}

/// Scripted Docker answers for `DeletionMonitor`. An unscripted `exists`
/// fails like an unreachable daemon; an unscripted `wait_destroyed` blocks like a quiet
/// event stream until cancelled.
#[derive(Default)]
pub(super) struct ScriptedEvents {
    destroyed: Option<WatchedContainer>,
    pub(super) streams: Mutex<VecDeque<Result<bool, String>>>,
    exists: Mutex<VecDeque<Result<bool, String>>>,
    calls: Mutex<Vec<String>>,
    cancelled: AtomicBool,
}

impl ScriptedEvents {
    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn wait_for_calls(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.calls.lock().unwrap().len() < count {
            assert!(
                Instant::now() < deadline,
                "calls so far: {:?}",
                self.calls()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn scripted<T>(queue: &Mutex<VecDeque<Result<T, String>>>) -> Option<Result<T>> {
    queue
        .lock()
        .unwrap()
        .pop_front()
        .map(|answer| answer.map_err(|e| anyhow!("{e}")))
}

impl ContainerEvents for ScriptedEvents {
    fn exists(&self, id: &str) -> Result<bool> {
        self.record(format!("exists:{id}"));
        scripted(&self.exists).unwrap_or_else(|| Err(anyhow!("Docker is unreachable")))
    }

    fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool> {
        self.record(format!("wait_destroyed:{}", container.id));
        if let Some(answer) = scripted(&self.streams) {
            return answer;
        }
        if self.destroyed.as_ref() == Some(container) {
            return Ok(true);
        }
        while !self.cancelled.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(false)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

fn monitor(events: &Arc<ScriptedEvents>, shutdown: &Shutdown) -> DeletionMonitor {
    DeletionMonitor::spawn(
        events.clone(),
        &watched("app-1"),
        Duration::from_millis(1),
        shutdown,
    )
}

fn replacement_before_monitoring(delayed_by_error: bool) {
    // The original is gone and a replacement has already taken its name. Event replay
    // must use the allocated ID and creation time, even if Docker initially fails.
    let events = Arc::new(ScriptedEvents {
        destroyed: Some(watched("app-1")),
        ..ScriptedEvents::default()
    });
    if delayed_by_error {
        events
            .streams
            .lock()
            .unwrap()
            .push_back(Err("Docker is unreachable".into()));
    }
    let (_sender, shutdown) = Shutdown::channel();
    let monitor = monitor(&events, &shutdown);
    let ended = shutdown.receiver.recv_timeout(Duration::from_secs(5));
    drop(monitor);
    assert_eq!(ended.unwrap(), Ended::DeletedElsewhere);
    assert!(events.calls().iter().all(|call| call.ends_with(":app-1")));
}

#[test]
fn deletion_monitor_uses_allocation_identity_when_replaced_before_monitoring() {
    replacement_before_monitoring(false);
}

#[test]
fn deletion_monitor_uses_allocation_identity_after_initial_docker_errors() {
    replacement_before_monitoring(true);
}

#[test]
fn deletion_monitor_reports_a_destroy_event() {
    let events = Arc::new(ScriptedEvents::default());
    events.streams.lock().unwrap().push_back(Ok(true));
    let (_sender, shutdown) = Shutdown::channel();
    let _monitor = monitor(&events, &shutdown);
    assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    assert_eq!(events.calls(), ["wait_destroyed:app-1"]);
}

#[test]
fn deletion_monitor_treats_docker_errors_as_unknown() {
    let events = Arc::new(ScriptedEvents::default());
    events
        .streams
        .lock()
        .unwrap()
        .extend([Err("Docker is unreachable".to_owned()), Ok(false)]);
    let (sender, shutdown) = Shutdown::channel();
    let monitor = monitor(&events, &shutdown);
    // Both reconciliations fail like an unreachable daemon; the third stream stays open.
    events.wait_for_calls(5);
    sender.send(Ended::Interrupted).unwrap();
    assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
    drop(monitor);
    assert_eq!(
        events.calls(),
        [
            "wait_destroyed:app-1",
            "exists:app-1",
            "wait_destroyed:app-1",
            "exists:app-1",
            "wait_destroyed:app-1",
        ]
    );
    assert!(shutdown.receiver.try_recv().is_err(), "reported a deletion");
}

#[test]
fn deletion_monitor_resubscribes_after_a_disconnect() {
    let events = Arc::new(ScriptedEvents::default());
    events.streams.lock().unwrap().extend([Ok(false), Ok(true)]);
    events.exists.lock().unwrap().push_back(Ok(true));
    let (_sender, shutdown) = Shutdown::channel();
    let _monitor = monitor(&events, &shutdown);
    assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    assert_eq!(
        events.calls(),
        [
            "wait_destroyed:app-1",
            "exists:app-1",
            "wait_destroyed:app-1",
        ]
    );
}

#[test]
fn deletion_monitor_follows_the_original_container_not_its_name() {
    // A disconnect hides the destroy event, and a new container already took the name.
    let events = Arc::new(ScriptedEvents::default());
    events.streams.lock().unwrap().push_back(Ok(false));
    events.exists.lock().unwrap().push_back(Ok(false));
    let (_sender, shutdown) = Shutdown::channel();
    let _monitor = monitor(&events, &shutdown);
    assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    assert_eq!(events.calls(), ["wait_destroyed:app-1", "exists:app-1",]);
}

#[test]
fn dropping_the_deletion_monitor_stops_the_watch() {
    let events = Arc::new(ScriptedEvents::default());
    let (_sender, shutdown) = Shutdown::channel();
    let monitor = monitor(&events, &shutdown);
    events.wait_for_calls(1);
    drop(monitor);
    assert!(events.cancelled.load(Ordering::SeqCst));
    assert!(shutdown.receiver.try_recv().is_err(), "reported a deletion");
}
