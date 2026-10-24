//! Remote streaming, cancellation, and retained cleanup regressions.
use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use tinybox_core::{ExecutionObserver, OutputStream};
use tokio::sync::Notify;

fn fixture_error(operation: &'static str) -> Error {
    Error::Backend {
        sandbox: "SSH test fixture".to_owned(),
        operation,
        message: "required fixture value is missing".to_owned(),
    }
}

fn last_arg<'a>(request: &'a ExecRequest, operation: &'static str) -> Result<&'a str> {
    request
        .argv
        .last()
        .map(String::as_str)
        .ok_or_else(|| fixture_error(operation))
}

fn is_cleanup_request(request: &ExecRequest) -> bool {
    request
        .argv
        .last()
        .is_some_and(|value| value.contains("kill -TERM -") && !value.contains("exec 3<&0"))
}

#[derive(Debug, Default)]
struct MockHost {
    requests: Mutex<Vec<ExecRequest>>,
    cancel_execution: AtomicBool,
    fail_cleanup: AtomicUsize,
    no_process_groups: AtomicBool,
    fail_prepare: AtomicBool,
    panic_observed: AtomicBool,
    observed_started: Notify,
    inner_cleanup_pending: AtomicBool,
    fail_inner_retry: AtomicBool,
    inner_retry_calls: AtomicUsize,
    fail_support_transport: AtomicBool,
    fail_support_with_255: AtomicBool,
    fail_prepare_transport: AtomicBool,
    fail_cleanup_transport: AtomicBool,
}

impl MockHost {
    fn requests(&self) -> Vec<ExecRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, request: &ExecRequest) {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
    }
}

#[async_trait]
impl Host for MockHost {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn run(&self, request: &ExecRequest) -> Result<ExecOutput> {
        self.record(request);
        let remote = request.argv.last().map(String::as_str).unwrap_or_default();
        if remote == "command -v setsid >/dev/null 2>&1" {
            if self.fail_support_transport.load(Ordering::SeqCst) {
                return Err(fixture_error("probe SSH transport"));
            }
            return Ok(ExecOutput::new(
                if self.fail_support_with_255.load(Ordering::SeqCst) {
                    255
                } else {
                    i32::from(self.no_process_groups.load(Ordering::SeqCst))
                },
                Vec::new(),
                Vec::new(),
            ));
        }
        if remote.starts_with("umask 077") {
            if self.fail_prepare_transport.load(Ordering::SeqCst) {
                return Err(fixture_error("prepare remote cleanup reservation"));
            }
            if self.fail_prepare.load(Ordering::SeqCst) {
                return Ok(ExecOutput::new(1, b"not-ready\n".to_vec(), Vec::new()));
            }
            return Ok(ExecOutput::new(0, b"ready\n".to_vec(), Vec::new()));
        }
        if remote.starts_with("dir=") {
            if self.fail_cleanup_transport.load(Ordering::SeqCst) {
                return Err(fixture_error("acknowledge remote cleanup"));
            }
            let remaining = self.fail_cleanup.load(Ordering::SeqCst);
            if remaining > 0
                && self
                    .fail_cleanup
                    .compare_exchange(remaining, remaining - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return Ok(ExecOutput::new(1, b"pending\n".to_vec(), Vec::new()));
            }
            return Ok(ExecOutput::new(0, b"clean\n".to_vec(), Vec::new()));
        }
        Ok(ExecOutput::new(0, Vec::new(), Vec::new()))
    }

    async fn run_observed(
        &self,
        request: &ExecRequest,
        observer: Arc<dyn ExecutionObserver>,
    ) -> Result<ExecOutput> {
        self.record(request);
        if self.panic_observed.load(Ordering::SeqCst) {
            std::panic::resume_unwind(Box::new("injected observed-provider panic"));
        }
        self.observed_started.notify_waiters();
        let stdout = [0, 255, 1, 2];
        observer.output(OutputStream::Stdout, &stdout)?;
        observer.output(OutputStream::Stderr, b"remote-error-bytes")?;
        if self.cancel_execution.load(Ordering::SeqCst) {
            observer.cancelled().await;
            return Err(Error::Backend {
                sandbox: "local".to_owned(),
                operation: "collect output",
                message: "execution cancelled".to_owned(),
            });
        }
        Ok(ExecOutput::new(
            7,
            stdout.to_vec(),
            b"remote-error-bytes".to_vec(),
        ))
    }

    fn has_pending_observed_cleanup(&self) -> bool {
        self.inner_cleanup_pending.load(Ordering::SeqCst)
    }

    async fn retry_observed_cleanup(&self) -> Result<()> {
        self.inner_retry_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_inner_retry.load(Ordering::SeqCst) {
            return Err(Error::ObservedCleanupPending {
                provider: "inner test host".to_owned(),
            });
        }
        self.inner_cleanup_pending.store(false, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct CollectingObserver {
    chunks: Mutex<Vec<(OutputStream, Vec<u8>)>>,
}

#[async_trait]
impl ExecutionObserver for CollectingObserver {
    fn output(&self, stream: OutputStream, bytes: &[u8]) -> Result<()> {
        self.chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((stream, bytes.to_vec()));
        Ok(())
    }
}

#[derive(Debug)]
struct CancelObserver {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancelObserver {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

#[async_trait]
impl ExecutionObserver for CancelObserver {
    fn output(&self, _: OutputStream, _: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.cancelled.load(Ordering::SeqCst) {
            notified.await;
        }
    }
}

#[tokio::test]
async fn observed_ssh_preserves_binary_pipes_and_execution_inputs() -> Result<()> {
    let inner = Arc::new(MockHost::default());
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);
    let observer = Arc::new(CollectingObserver::default());
    let request = ExecRequest::new(["python", "-c", "print(1)"])
        .with_cwd("/workspace/build")
        .with_env("TOKEN", "opaque")
        .with_stdin([0, 1, 255]);

    let output = host.run_observed(&request, observer.clone()).await?;

    assert_eq!(output.exit_code, 7);
    assert_eq!(output.stdout, [0, 255, 1, 2]);
    assert_eq!(output.stderr, b"remote-error-bytes");
    assert_eq!(
        observer
            .chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_slice(),
        &[
            (OutputStream::Stdout, vec![0, 255, 1, 2]),
            (OutputStream::Stderr, b"remote-error-bytes".to_vec())
        ]
    );
    let requests = inner.requests();
    let launch = requests
        .iter()
        .find(|request| request.stdin.is_some())
        .ok_or_else(|| fixture_error("find supervised SSH request"))?;
    assert_eq!(launch.stdin.as_deref(), Some([0, 1, 255].as_slice()));
    let script = last_arg(launch, "read supervised SSH script")?;
    assert!(script.contains("setsid"));
    assert!(script.contains("/workspace/build"));
    assert!(script.contains("TOKEN=opaque"));
    let cleanup = requests
        .iter()
        .find(|request| is_cleanup_request(request))
        .ok_or_else(|| fixture_error("find remote process group cleanup"))?;
    let cleanup = last_arg(cleanup, "read remote cleanup script")?;
    assert!(cleanup.contains("kill -TERM -"));
    assert!(cleanup.contains("kill -KILL -"));
    assert!(cleanup.contains("clean\\n"));
    Ok(())
}

#[tokio::test]
async fn cancellation_waits_for_remote_group_cleanup_acknowledgement() -> Result<()> {
    let inner = Arc::new(MockHost {
        cancel_execution: AtomicBool::new(true),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);
    let observer = Arc::new(CancelObserver {
        cancelled: AtomicBool::new(false),
        notify: Notify::new(),
    });
    let cancel = observer.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancel.cancel();
    });

    let result = host
        .run_observed(&ExecRequest::new(["sleep", "600"]), observer)
        .await;
    cancel_task.await.map_err(|error| Error::Backend {
        sandbox: "test".to_owned(),
        operation: "join cancellation fixture",
        message: error.to_string(),
    })?;

    assert!(matches!(result, Err(Error::Backend { .. })));
    assert!(!host.has_pending_observed_cleanup());
    let requests = inner.requests();
    let cleanup_index = requests
        .iter()
        .position(is_cleanup_request)
        .ok_or_else(|| fixture_error("find cleanup SSH invocation"))?;
    let execution_index = requests
        .iter()
        .position(|request| {
            request
                .argv
                .last()
                .is_some_and(|value| value.contains("exec setsid"))
        })
        .ok_or_else(|| fixture_error("find supervised remote command"))?;
    assert!(cleanup_index > execution_index);
    Ok(())
}

#[tokio::test]
async fn failed_remote_kill_keeps_a_retryable_cleanup_handle() -> Result<()> {
    let inner = Arc::new(MockHost {
        cancel_execution: AtomicBool::new(true),
        fail_cleanup: AtomicUsize::new(2),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);
    let observer = Arc::new(CancelObserver {
        cancelled: AtomicBool::new(false),
        notify: Notify::new(),
    });
    let cancel = observer.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancel.cancel();
    });

    assert!(matches!(
        host.run_observed(&ExecRequest::new(["sleep", "600"]), observer)
            .await,
        Err(Error::ObservedCleanupPending { .. })
    ));
    cancel_task.await.map_err(|error| Error::Backend {
        sandbox: "test".to_owned(),
        operation: "join cleanup retry fixture",
        message: error.to_string(),
    })?;
    assert!(host.has_pending_observed_cleanup());

    host.retry_observed_cleanup().await?;

    assert!(!host.has_pending_observed_cleanup());
    assert_eq!(
        inner
            .requests()
            .iter()
            .filter(|request| is_cleanup_request(request))
            .count(),
        3,
        "two failed attempts are followed by one explicit retained-handle retry"
    );
    Ok(())
}

#[tokio::test]
async fn cleanup_retry_attempts_remote_and_inner_owners_even_when_one_fails() -> Result<()> {
    let inner = Arc::new(MockHost {
        inner_cleanup_pending: AtomicBool::new(true),
        fail_inner_retry: AtomicBool::new(true),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);
    let (remote_id, _) = host.observed_cleanup.reserve();
    host.observed_cleanup.mark_pending(&remote_id);

    assert!(host.has_pending_observed_cleanup());
    assert!(matches!(
        host.retry_observed_cleanup().await,
        Err(Error::ObservedCleanupPending { .. })
    ));
    assert_eq!(inner.inner_retry_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        inner
            .requests()
            .iter()
            .filter(|request| is_cleanup_request(request))
            .count(),
        1,
        "remote cleanup is attempted despite the inner cleanup failure"
    );
    assert!(!host.observed_cleanup.has_pending());
    assert!(host.has_pending_observed_cleanup());

    inner.fail_inner_retry.store(false, Ordering::SeqCst);
    host.retry_observed_cleanup().await?;
    assert_eq!(inner.inner_retry_calls.load(Ordering::SeqCst), 2);
    assert!(!host.has_pending_observed_cleanup());
    Ok(())
}

#[tokio::test]
async fn inner_cleanup_retry_runs_even_when_remote_cleanup_fails() -> Result<()> {
    let inner = Arc::new(MockHost {
        fail_cleanup: AtomicUsize::new(1),
        inner_cleanup_pending: AtomicBool::new(true),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);
    let (remote_id, _) = host.observed_cleanup.reserve();
    host.observed_cleanup.mark_pending(&remote_id);

    assert!(matches!(
        host.retry_observed_cleanup().await,
        Err(Error::ObservedCleanupPending { .. })
    ));
    assert_eq!(inner.inner_retry_calls.load(Ordering::SeqCst), 1);
    assert!(host.observed_cleanup.has_pending());
    assert!(!inner.has_pending_observed_cleanup());
    assert!(host.has_pending_observed_cleanup());

    host.retry_observed_cleanup().await?;
    assert_eq!(inner.inner_retry_calls.load(Ordering::SeqCst), 2);
    assert!(!host.has_pending_observed_cleanup());
    Ok(())
}

#[tokio::test]
async fn dropping_the_caller_keeps_the_remote_cleanup_owner_alive() -> Result<()> {
    let inner = Arc::new(MockHost {
        cancel_execution: AtomicBool::new(true),
        fail_cleanup: AtomicUsize::new(2),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);
    let observer = Arc::new(CancelObserver {
        cancelled: AtomicBool::new(false),
        notify: Notify::new(),
    });
    let started = inner.observed_started.notified();
    tokio::pin!(started);
    started.as_mut().enable();
    let caller_host = host.clone();
    let caller_observer = observer.clone();
    let caller = tokio::spawn(async move {
        caller_host
            .run_observed(&ExecRequest::new(["sleep", "600"]), caller_observer)
            .await
    });
    started.await;
    caller.abort();
    observer.cancel();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !host.has_pending_observed_cleanup() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|error| Error::Backend {
        sandbox: "test".to_owned(),
        operation: "await retained SSH cleanup owner",
        message: error.to_string(),
    })?;
    host.retry_observed_cleanup().await?;
    assert!(!host.has_pending_observed_cleanup());
    Ok(())
}

#[derive(Debug)]
struct RefusingObserver;

#[async_trait]
impl ExecutionObserver for RefusingObserver {
    fn output(&self, _: OutputStream, _: &[u8]) -> Result<()> {
        Err(Error::io(
            "deliver SSH output fixture",
            &std::io::Error::other("receiver closed"),
        ))
    }
}

#[tokio::test]
async fn output_refusal_triggers_remote_cleanup_before_returning_the_error() -> Result<()> {
    let inner = Arc::new(MockHost {
        cancel_execution: AtomicBool::new(true),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);

    let result = host
        .run_observed(
            &ExecRequest::new(["sleep", "600"]),
            Arc::new(RefusingObserver),
        )
        .await;

    assert!(matches!(
        result,
        Err(Error::Io {
            operation: "deliver SSH output fixture",
            ..
        })
    ));
    assert!(!host.has_pending_observed_cleanup());
    assert!(inner.requests().iter().any(is_cleanup_request));
    Ok(())
}

#[derive(Debug)]
struct BlockingObserver {
    entered: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
    first: AtomicBool,
}

#[async_trait]
impl ExecutionObserver for BlockingObserver {
    fn output(&self, _: OutputStream, _: &[u8]) -> Result<()> {
        if !self.first.swap(true, Ordering::SeqCst) {
            self.entered.wait();
            self.release.wait();
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_provider_obeys_synchronous_output_backpressure() -> Result<()> {
    let inner = Arc::new(MockHost::default());
    let host = SshHost::new(inner, SshTarget::new("builder")?);
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let observer = Arc::new(BlockingObserver {
        entered: entered.clone(),
        release: release.clone(),
        first: AtomicBool::new(false),
    });
    let task = tokio::spawn(async move {
        host.run_observed(&ExecRequest::new(["producer"]), observer)
            .await
    });

    tokio::task::spawn_blocking(move || entered.wait())
        .await
        .map_err(|error| Error::Backend {
            sandbox: "test".to_owned(),
            operation: "join output backpressure fixture",
            message: error.to_string(),
        })?;
    assert!(
        !task.is_finished(),
        "command completion waits for output acceptance"
    );
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .map_err(|error| Error::Backend {
            sandbox: "test".to_owned(),
            operation: "release output backpressure fixture",
            message: error.to_string(),
        })?;
    let output = task.await.map_err(|error| Error::Backend {
        sandbox: "test".to_owned(),
        operation: "join observed output fixture",
        message: error.to_string(),
    })??;
    assert_eq!(output.exit_code, 7, "non-zero command status is a result");
    Ok(())
}

#[tokio::test]
async fn remote_process_group_support_is_checked_before_execution() -> Result<()> {
    let inner = Arc::new(MockHost {
        no_process_groups: AtomicBool::new(true),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);
    let result = host
        .run_observed(
            &ExecRequest::new(["sleep", "600"]),
            Arc::new(CollectingObserver::default()),
        )
        .await;
    assert!(matches!(result, Err(Error::UnsupportedStreaming { .. })));
    assert_eq!(inner.requests().len(), 1, "only the capability probe runs");
    Ok(())
}

#[tokio::test]
async fn empty_command_is_rejected_before_remote_probes() -> Result<()> {
    let inner = Arc::new(MockHost::default());
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);

    assert!(matches!(
        host.run_observed(
            &ExecRequest::new(std::iter::empty::<&str>()),
            Arc::new(CollectingObserver::default())
        )
        .await,
        Err(Error::EmptyCommand { .. })
    ));
    assert_eq!(inner.requests().len(), 0);
    Ok(())
}

#[tokio::test]
async fn support_probe_transport_failures_are_reported_before_reservation() -> Result<()> {
    for inner in [
        Arc::new(MockHost {
            fail_support_transport: AtomicBool::new(true),
            ..MockHost::default()
        }),
        Arc::new(MockHost {
            fail_support_with_255: AtomicBool::new(true),
            ..MockHost::default()
        }),
    ] {
        let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);
        assert!(matches!(
            host.run_observed(
                &ExecRequest::new(["echo", "safe"]),
                Arc::new(CollectingObserver::default())
            )
            .await,
            Err(Error::Backend { .. })
        ));
        assert_eq!(inner.requests().len(), 1);
        assert!(!host.has_pending_observed_cleanup());
    }
    Ok(())
}

#[tokio::test]
async fn prepare_transport_error_is_returned_after_successful_reservation_cleanup() -> Result<()> {
    let inner = Arc::new(MockHost {
        fail_prepare_transport: AtomicBool::new(true),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);

    assert!(matches!(
        host.run_observed(
            &ExecRequest::new(["echo", "safe"]),
            Arc::new(CollectingObserver::default())
        )
        .await,
        Err(Error::Backend { .. })
    ));
    assert!(inner.requests().iter().any(is_cleanup_request));
    assert!(!host.has_pending_observed_cleanup());
    Ok(())
}

#[tokio::test]
async fn malformed_reservation_response_is_cleaned_before_returning() -> Result<()> {
    let inner = Arc::new(MockHost {
        fail_prepare: AtomicBool::new(true),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);

    assert!(matches!(
        host.run_observed(
            &ExecRequest::new(["echo", "safe"]),
            Arc::new(CollectingObserver::default())
        )
        .await,
        Err(Error::Backend {
            operation: "reserve remote execution cleanup",
            ..
        })
    ));
    assert!(inner.requests().iter().any(is_cleanup_request));
    assert!(!host.has_pending_observed_cleanup());
    Ok(())
}

#[tokio::test]
async fn invalid_remote_command_request_releases_its_reservation() -> Result<()> {
    let inner = Arc::new(MockHost::default());
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);

    assert!(matches!(
        host.run_observed_owned(
            &ExecRequest::new(std::iter::empty::<&str>()),
            Arc::new(CollectingObserver::default())
        )
        .await,
        Err(Error::EmptyCommand { .. })
    ));
    assert!(!host.has_pending_observed_cleanup());
    Ok(())
}

#[tokio::test]
async fn failed_reservation_acknowledgement_retains_cleanup_for_retry() -> Result<()> {
    let inner = Arc::new(MockHost {
        fail_cleanup: AtomicUsize::new(1),
        fail_prepare: AtomicBool::new(true),
        ..MockHost::default()
    });
    let host = SshHost::new(inner.clone(), SshTarget::new("builder")?);

    assert!(matches!(
        host.run_observed(
            &ExecRequest::new(["sleep", "600"]),
            Arc::new(CollectingObserver::default())
        )
        .await,
        Err(Error::ObservedCleanupPending { .. })
    ));
    assert!(host.has_pending_observed_cleanup());

    host.retry_observed_cleanup().await?;

    assert!(!host.has_pending_observed_cleanup());
    assert_eq!(
        inner
            .requests()
            .iter()
            .filter(|request| is_cleanup_request(request))
            .count(),
        2,
        "one failed reservation cleanup is followed by one explicit retry"
    );
    Ok(())
}

#[tokio::test]
async fn a_provider_panic_retains_the_remote_cleanup_reservation() -> Result<()> {
    let inner = Arc::new(MockHost {
        panic_observed: AtomicBool::new(true),
        ..MockHost::default()
    });
    let host = SshHost::new(inner, SshTarget::new("builder")?);

    assert!(matches!(
        host.run_observed(
            &ExecRequest::new(["sleep", "600"]),
            Arc::new(CollectingObserver::default())
        )
        .await,
        Err(Error::Backend { .. })
    ));
    assert!(host.has_pending_observed_cleanup());

    host.retry_observed_cleanup().await?;

    assert!(!host.has_pending_observed_cleanup());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn generated_remote_supervisor_kills_and_waits_for_its_process_group() -> Result<()> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let prepare = observed::prepare_request(&target, &id, &directory);
    let prepare_script = last_arg(&prepare, "read remote setup script")?;
    let prepared = Command::new("/bin/sh")
        .arg("-c")
        .arg(prepare_script)
        .output()
        .map_err(|error| Error::io("reserve local remote-protocol fixture", &error))?;
    assert!(prepared.status.success());
    assert_eq!(prepared.stdout, b"ready\n");
    let launch = observed::request(
        &target,
        &ExecRequest::new(["/bin/sh", "-c", "printf ready; exec sleep 60"]),
        &id,
        &directory,
    )?;
    let pid_file = format!("{directory}/pid");
    let remote_script = last_arg(&launch, "read remote launch script")?;
    let mut process = Command::new("/bin/sh")
        .arg("-c")
        .arg(remote_script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::io("start local SSH protocol fixture", &error))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::metadata(&pid_file).map_or(true, |metadata| metadata.len() == 0)
        && Instant::now() < deadline
    {
        if let Some(status) = process
            .try_wait()
            .map_err(|error| Error::io("check local SSH protocol fixture", &error))?
        {
            return Err(Error::Backend {
                sandbox: "ssh test".to_owned(),
                operation: "start remote supervisor fixture",
                message: format!("wrapper exited early with {status}"),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid_text = std::fs::read_to_string(&pid_file)
        .map_err(|error| Error::io("read remote supervisor pid", &error))?;
    let pid = pid_text.trim();
    assert!(!pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()));

    let cleanup = observed::cleanup_request(&target, &id, &directory, true);
    let cleanup_script = last_arg(&cleanup, "read remote cleanup script")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("run local remote-cleanup fixture", &error))?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"clean\n");
    let status = process
        .wait()
        .map_err(|error| Error::io("wait for remote supervisor fixture", &error))?;
    assert!(
        !status.success(),
        "the cancelled command must not report success"
    );
    let group_alive = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -0 -{pid} 2>/dev/null"))
        .status()
        .map_err(|error| Error::io("check remote process group", &error))?
        .success();
    assert!(
        !group_alive,
        "cleanup acknowledgement follows group exit and wait"
    );
    state.remove(&id);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn generated_remote_supervisor_reaps_background_descendants_before_acknowledging() -> Result<()> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let prepare = observed::prepare_request(&target, &id, &directory);
    let prepare_script = last_arg(&prepare, "read remote setup script")?;
    let prepared = Command::new("/bin/sh")
        .arg("-c")
        .arg(prepare_script)
        .output()
        .map_err(|error| Error::io("reserve local descendant fixture", &error))?;
    assert!(prepared.status.success());

    let launch = observed::request(
        &target,
        &ExecRequest::new(["/bin/sh", "-c", "sleep 600 & sleep 0.05; exit 0"]),
        &id,
        &directory,
    )?;
    let pid_file = format!("{directory}/pid");
    let remote_script = last_arg(&launch, "read remote launch script")?;
    let mut process = Command::new("/bin/sh")
        .arg("-c")
        .arg(remote_script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::io("start descendant protocol fixture", &error))?;
    let mut stdout = process
        .stdout
        .take()
        .ok_or_else(|| fixture_error("take descendant fixture stdout"))?;
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let read = stdout.read_to_end(&mut output);
        let _ = sender.send((read, output));
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::metadata(&pid_file).map_or(true, |metadata| metadata.len() == 0)
        && Instant::now() < deadline
    {
        if let Some(status) = process
            .try_wait()
            .map_err(|error| Error::io("check descendant protocol fixture", &error))?
        {
            return Err(Error::Backend {
                sandbox: "ssh test".to_owned(),
                operation: "start descendant supervisor fixture",
                message: format!("wrapper exited before recording its pid: {status}"),
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid_text = std::fs::read_to_string(&pid_file)
        .map_err(|error| Error::io("read descendant supervisor pid", &error))?;
    let pid = pid_text.trim();
    assert!(!pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()));

    let status = process
        .wait()
        .map_err(|error| Error::io("wait for descendant supervisor", &error))?;
    assert!(
        status.success(),
        "the foreground command exits successfully"
    );
    let pipe_released = receiver.recv_timeout(Duration::from_secs(1)).ok();
    let group_alive = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -0 -{pid} 2>/dev/null"))
        .status()
        .map_err(|error| Error::io("check descendant process group", &error))?
        .success();
    if group_alive {
        let _ = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("kill -KILL -{pid} 2>/dev/null || true"))
            .status();
    }
    let _ = reader.join();
    assert!(
        pipe_released.is_some(),
        "background descendants must not retain the remote output pipe after acknowledgement"
    );
    if let Some((read, _)) = pipe_released {
        read.map_err(|error| Error::io("read descendant fixture output", &error))?;
    }
    assert!(
        !group_alive,
        "supervisor must terminate group descendants before its completion acknowledgement"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn generated_remote_supervisor_forwards_binary_stdin_to_the_foreground_command() -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let prepare = observed::prepare_request(&target, &id, &directory);
    let prepare_script = last_arg(&prepare, "read remote setup script")?;
    let prepared = Command::new("/bin/sh")
        .arg("-c")
        .arg(prepare_script)
        .output()
        .map_err(|error| Error::io("reserve local stdin fixture", &error))?;
    assert!(prepared.status.success());

    let payload = [0, 1, 255, 0, 42];
    let launch = observed::request(&target, &ExecRequest::new(["/bin/cat"]), &id, &directory)?;
    let remote_script = last_arg(&launch, "read remote launch script")?;
    let mut process = Command::new("/bin/sh")
        .arg("-c")
        .arg(remote_script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::io("start local stdin protocol fixture", &error))?;
    process
        .stdin
        .take()
        .ok_or_else(|| fixture_error("take local stdin fixture"))?
        .write_all(&payload)
        .map_err(|error| Error::io("write local binary stdin fixture", &error))?;
    let output = process
        .wait_with_output()
        .map_err(|error| Error::io("wait for local stdin protocol fixture", &error))?;
    assert!(output.status.success());
    assert_eq!(output.stdout, payload);

    let cleanup = observed::cleanup_request(&target, &id, &directory, true);
    let cleanup_script = last_arg(&cleanup, "read local stdin fixture cleanup")?;
    let cleaned = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("clean local stdin protocol fixture", &error))?;
    assert!(cleaned.status.success());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_reclaims_an_abandoned_directory_after_its_process_group_is_gone() -> Result<()> {
    use std::process::Command;

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    std::fs::create_dir(&directory)
        .map_err(|error| Error::io("create abandoned SSH directory fixture", &error))?;
    std::fs::write(format!("{directory}/owner"), format!("{id}\n"))
        .map_err(|error| Error::io("write abandoned SSH owner fixture", &error))?;
    let mut exited = Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .map_err(|error| Error::io("start exited SSH process fixture", &error))?;
    let dead_pid = exited.id();
    let status = exited
        .wait()
        .map_err(|error| Error::io("reap exited SSH process fixture", &error))?;
    assert!(status.success());
    std::fs::write(format!("{directory}/pid"), format!("{dead_pid}\n"))
        .map_err(|error| Error::io("write abandoned SSH group fixture", &error))?;
    std::fs::write(format!("{directory}/wrapper"), format!("{dead_pid}\n"))
        .map_err(|error| Error::io("write abandoned SSH wrapper fixture", &error))?;

    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_script = last_arg(&cleanup, "read abandoned SSH cleanup script")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("run abandoned SSH cleanup fixture", &error))?;

    assert!(
        output.status.success(),
        "cleanup should recover an owner-marked directory after both wrapper and group exit: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(output.stdout, b"clean\n");
    assert!(!std::path::Path::new(&directory).exists());
    state.remove(&id);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_keeps_non_directory_claim_conflicts_pending() -> Result<()> {
    use std::process::Command;

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    std::fs::create_dir(&directory)
        .map_err(|error| Error::io("create claim-conflict SSH directory", &error))?;
    std::fs::write(format!("{directory}/owner"), format!("{id}\n"))
        .map_err(|error| Error::io("write claim-conflict SSH owner", &error))?;
    let claim = format!("{directory}.cleanup");
    std::fs::write(&claim, b"unowned conflicting claim")
        .map_err(|error| Error::io("create conflicting SSH claim", &error))?;

    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_script = last_arg(&cleanup, "read claim-conflict cleanup script")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("run claim-conflict cleanup fixture", &error))?;

    assert!(!output.status.success());
    assert_eq!(output.stdout, b"pending\n");
    assert!(std::path::Path::new(&directory).is_dir());
    assert!(std::path::Path::new(&claim).is_file());
    std::fs::remove_file(&claim)
        .map_err(|error| Error::io("remove conflicting SSH claim", &error))?;
    state.remove(&id);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_preserves_a_live_wrapper_and_retries_an_interrupted_claim() -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    std::fs::create_dir(&directory)
        .map_err(|error| Error::io("create live SSH wrapper fixture", &error))?;
    std::fs::write(format!("{directory}/owner"), format!("{id}\n"))
        .map_err(|error| Error::io("write live SSH owner fixture", &error))?;
    let mut exited = Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .map_err(|error| Error::io("start exited SSH group fixture", &error))?;
    let dead_group = exited.id();
    exited
        .wait()
        .map_err(|error| Error::io("reap exited SSH group fixture", &error))?;
    std::fs::write(format!("{directory}/pid"), format!("{dead_group}\n"))
        .map_err(|error| Error::io("write exited SSH group fixture", &error))?;

    let wrapper_file = format!("{directory}/wrapper");
    let mut live_wrapper = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "printf '%s\\n' \"$$\" > {}; read input; printf done > {}; : {id}",
            tinybox_core::shell::quote(&wrapper_file),
            tinybox_core::shell::quote(&format!("{directory}/after-cleanup"))
        ))
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| Error::io("start live SSH wrapper fixture", &error))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !std::path::Path::new(&wrapper_file).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(std::path::Path::new(&wrapper_file).exists());

    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_script = last_arg(&cleanup, "read live-wrapper cleanup script")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("run live-wrapper cleanup fixture", &error))?;
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"pending\n");
    assert!(std::path::Path::new(&directory).exists());
    assert!(!std::path::Path::new(&format!("{directory}.cleanup")).exists());

    live_wrapper
        .stdin
        .take()
        .ok_or_else(|| fixture_error("take live wrapper input"))?
        .write_all(b"continue\n")
        .map_err(|error| Error::io("continue live wrapper fixture", &error))?;
    let status = live_wrapper
        .wait()
        .map_err(|error| Error::io("wait for preserved live wrapper", &error))?;
    assert!(status.success());
    assert!(std::path::Path::new(&format!("{directory}/after-cleanup")).exists());

    std::fs::rename(&directory, format!("{directory}.cleanup"))
        .map_err(|error| Error::io("simulate interrupted SSH cleanup claim", &error))?;

    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_script = last_arg(&cleanup, "read retry cleanup script")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("retry interrupted SSH cleanup fixture", &error))?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"clean\n");
    assert!(!std::path::Path::new(&directory).exists());
    assert!(!std::path::Path::new(&format!("{directory}.cleanup")).exists());
    state.remove(&id);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_rechecks_wrapper_identity_after_claiming_an_unpublished_request() -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    std::fs::create_dir(&directory)
        .map_err(|error| Error::io("create unpublished SSH directory", &error))?;
    std::fs::write(format!("{directory}/owner"), format!("{id}\n"))
        .map_err(|error| Error::io("write unpublished SSH owner", &error))?;
    let exited = Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .status()
        .map_err(|error| Error::io("start exited unpublished-group fixture", &error))?;
    assert!(exited.success());
    std::fs::write(format!("{directory}/pid"), "2147483647\n")
        .map_err(|error| Error::io("write unpublished SSH group fixture", &error))?;

    let mut late_wrapper = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("read input; : {id}"))
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| Error::io("start unpublished SSH wrapper", &error))?;
    let wrapper_pid = late_wrapper.id();
    let helper = format!("{directory}.helpers");
    std::fs::create_dir(&helper)
        .map_err(|error| Error::io("create SSH cleanup helper directory", &error))?;
    let moved_marker = format!("{helper}/moved");
    let fake_mv = format!("{helper}/mv");
    std::fs::write(
        &fake_mv,
        format!(
            "#!/bin/sh\n/bin/mv \"$1\" \"$2\" || exit $?\nprintf '%s\\n' {wrapper_pid} > \"$2/wrapper\"\nprintf moved > {}\n",
            tinybox_core::shell::quote(&moved_marker)
        ),
    )
    .map_err(|error| Error::io("write late-wrapper mv fixture", &error))?;
    let mut permissions = std::fs::metadata(&fake_mv)
        .map_err(|error| Error::io("inspect late-wrapper mv fixture", &error))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_mv, permissions)
        .map_err(|error| Error::io("make late-wrapper mv fixture executable", &error))?;

    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_script = last_arg(&cleanup, "read unpublished-request cleanup script")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .env(
            "PATH",
            format!("{helper}:{}", std::env::var("PATH").unwrap_or_default()),
        )
        .output()
        .map_err(|error| Error::io("claim unpublished SSH directory", &error))?;

    assert!(!output.status.success());
    assert_eq!(output.stdout, b"pending\n");
    assert!(std::path::Path::new(&moved_marker).exists());
    assert!(std::path::Path::new(&directory).is_dir());
    assert!(!std::path::Path::new(&format!("{directory}.cleanup")).exists());

    late_wrapper
        .stdin
        .take()
        .ok_or_else(|| fixture_error("take unpublished wrapper stdin"))?
        .write_all(b"continue\n")
        .map_err(|error| Error::io("continue unpublished SSH wrapper", &error))?;
    assert!(
        late_wrapper
            .wait()
            .map_err(|error| Error::io("wait for unpublished SSH wrapper", &error))?
            .success()
    );

    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_script = last_arg(&cleanup, "read retry unpublished cleanup script")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("retry unpublished SSH cleanup", &error))?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"clean\n");
    state.remove(&id);
    let _ = std::fs::remove_dir_all(helper);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_stops_the_owned_group_after_its_wrapper_dies() -> Result<()> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let prepare = observed::prepare_request(&target, &id, &directory);
    let prepare_script = last_arg(&prepare, "read orphaned-group setup")?;
    let prepared = Command::new("/bin/sh")
        .arg("-c")
        .arg(prepare_script)
        .output()
        .map_err(|error| Error::io("reserve orphaned-group fixture", &error))?;
    assert!(prepared.status.success());

    let launch = observed::request(
        &target,
        &ExecRequest::new(["/bin/sh", "-c", "sleep 600"]),
        &id,
        &directory,
    )?;
    let launch_script = last_arg(&launch, "read orphaned-group launcher")?;
    let mut wrapper = Command::new("/bin/sh")
        .arg("-c")
        .arg(launch_script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::io("start orphaned-group wrapper", &error))?;
    let pid_file = format!("{directory}/pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while std::fs::metadata(&pid_file).map_or(true, |metadata| metadata.len() == 0)
        && Instant::now() < deadline
    {
        if let Some(status) = wrapper
            .try_wait()
            .map_err(|error| Error::io("check orphaned-group wrapper", &error))?
        {
            return Err(Error::Backend {
                sandbox: "ssh test".to_owned(),
                operation: "start orphaned-group fixture",
                message: format!("wrapper exited before writing its group id: {status}"),
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let group = std::fs::read_to_string(&pid_file)
        .map_err(|error| Error::io("read orphaned process group", &error))?;
    let group = group.trim();
    assert!(!group.is_empty() && group.bytes().all(|byte| byte.is_ascii_digit()));

    wrapper
        .kill()
        .map_err(|error| Error::io("kill orphaned-group wrapper", &error))?;
    wrapper
        .wait()
        .map_err(|error| Error::io("reap orphaned-group wrapper", &error))?;

    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_script = last_arg(&cleanup, "read orphaned-group cleanup")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("run orphaned-group cleanup", &error))?;
    assert!(
        output.status.success(),
        "owned process group should be stopped before cleanup is acknowledged: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(output.stdout, b"clean\n");
    assert!(!std::path::Path::new(&directory).exists());
    let group_alive = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -0 -{group} 2>/dev/null"))
        .status()
        .map_err(|error| Error::io("verify orphaned process group stopped", &error))?
        .success();
    assert!(!group_alive, "cleanup must stop the orphaned group");
    state.remove(&id);
    Ok(())
}

#[cfg(target_os = "linux")]
fn wait_for_nonempty_marker(
    path: &str,
    wrapper: &mut std::process::Child,
    operation: &'static str,
) -> Result<bool> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(2);
    while std::fs::metadata(path).map_or(true, |metadata| metadata.len() == 0)
        && Instant::now() < deadline
    {
        if let Some(status) = wrapper
            .try_wait()
            .map_err(|error| Error::io("poll generated SSH wrapper", &error))?
        {
            return Err(Error::Backend {
                sandbox: "SSH test fixture".to_owned(),
                operation,
                message: format!("wrapper exited before publishing the marker: {status}"),
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0))
}

#[cfg(target_os = "linux")]
fn start_term_ignoring_group(directory: &str, id: &str) -> Result<(String, String)> {
    use std::process::{Command, Stdio};

    let target = SshTarget::new("builder")?;
    let prepare = observed::prepare_request(&target, id, directory);
    let setup = Command::new("/bin/sh")
        .arg("-c")
        .arg(last_arg(&prepare, "read generated cleanup setup")?)
        .output()
        .map_err(|error| Error::io("prepare generated cleanup fixture", &error))?;
    assert!(setup.status.success());

    let term_marker = format!("/tmp/tinybox-observed-term-{id}");
    let ready_marker = format!("{directory}/command-ready");
    let workload = format!(
        "trap 'printf term > {}; exit 0' TERM; /bin/sh -c 'trap \"\" TERM; printf ready > {}; exec sleep 600' & wait",
        tinybox_core::shell::quote(&term_marker),
        tinybox_core::shell::quote(&ready_marker)
    );
    let request = observed::request(
        &target,
        &ExecRequest::new(["/bin/sh", "-c", &workload]),
        id,
        directory,
    )?;
    let mut wrapper = Command::new("/bin/sh")
        .arg("-c")
        .arg(last_arg(&request, "read generated cleanup request")?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::io("start generated cleanup request", &error))?;
    let pid_file = format!("{directory}/pid");
    if !wait_for_nonempty_marker(&pid_file, &mut wrapper, "start generated cleanup request")? {
        let _ = wrapper.kill();
        let _ = wrapper.wait();
        return Err(Error::Backend {
            sandbox: "SSH test fixture".to_owned(),
            operation: "start generated cleanup request",
            message: "generated request did not publish its group id".to_owned(),
        });
    }
    if !wait_for_nonempty_marker(&ready_marker, &mut wrapper, "start generated user command")? {
        if let Ok(group) = std::fs::read_to_string(&pid_file) {
            let group = group.trim();
            if !group.is_empty() && group.bytes().all(|byte| byte.is_ascii_digit()) {
                let _ = Command::new("/bin/sh")
                    .arg("-c")
                    .arg(format!("kill -KILL -{group} 2>/dev/null || true"))
                    .status();
            }
        }
        let _ = wrapper.kill();
        let _ = wrapper.wait();
        return Err(Error::Backend {
            sandbox: "SSH test fixture".to_owned(),
            operation: "start generated user command",
            message: "TERM-ignoring child did not publish readiness".to_owned(),
        });
    }
    let group = std::fs::read_to_string(&pid_file)
        .map_err(|error| Error::io("read generated cleanup group id", &error))?
        .trim()
        .to_owned();
    let leader_info = Command::new("ps")
        .args(["-ww", "-p", &group, "-o", "pid=,pgid=,sid=,command="])
        .output()
        .map_err(|error| Error::io("inspect generated cleanup anchor", &error))?;
    assert!(
        String::from_utf8_lossy(&leader_info.stdout).contains(id),
        "generated group anchor did not carry its UUID: {}",
        String::from_utf8_lossy(&leader_info.stdout)
    );
    wrapper
        .kill()
        .map_err(|error| Error::io("kill generated request wrapper", &error))?;
    wrapper
        .wait()
        .map_err(|error| Error::io("reap generated request wrapper", &error))?;
    Ok((group, term_marker))
}

#[cfg(target_os = "linux")]
fn finish_term_ignoring_cleanup(
    mut cleanup_process: std::process::Child,
    group: &str,
) -> Result<(std::process::Output, bool, bool)> {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(12);
    let mut cleanup_finished = false;
    while Instant::now() < deadline {
        if cleanup_process
            .try_wait()
            .map_err(|error| Error::io("poll completed TERM-resistant cleanup", &error))?
            .is_some()
        {
            cleanup_finished = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !cleanup_finished {
        let _ = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("kill -KILL -{group} 2>/dev/null || true"))
            .status();
        let _ = cleanup_process.kill();
    }
    let output = cleanup_process
        .wait_with_output()
        .map_err(|error| Error::io("collect TERM-resistant cleanup result", &error))?;
    let group_alive = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -0 -{group} 2>/dev/null"))
        .status()
        .map_err(|error| Error::io("check TERM-resistant process group", &error))?
        .success();
    if group_alive {
        let _ = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("kill -KILL -{group} 2>/dev/null || true"))
            .status();
    }
    Ok((output, cleanup_finished, group_alive))
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_kills_a_term_ignoring_descendant_while_its_uuid_anchor_stays_owned() -> Result<()> {
    use std::process::{Command, Stdio};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let (group, term_marker) = start_term_ignoring_group(&directory, &id)?;

    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_process = Command::new("/bin/sh")
        .arg("-c")
        .arg(last_arg(&cleanup, "read TERM-resistant group cleanup")?)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::io("start TERM-resistant cleanup", &error))?;
    let (output, cleanup_finished, group_alive) =
        finish_term_ignoring_cleanup(cleanup_process, &group)?;

    assert!(
        cleanup_finished,
        "cleanup did not finish its bounded escalation"
    );
    assert!(
        output.status.success(),
        "cleanup must kill the remaining owned session after its wrapper exits: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(output.stdout, b"clean\n");
    let term_received = std::fs::read_to_string(&term_marker)
        .map_err(|error| Error::io("read TERM delivery marker", &error))?;
    assert_eq!(term_received, "term");
    std::fs::remove_file(term_marker)
        .map_err(|error| Error::io("remove TERM delivery marker", &error))?;
    assert!(
        !group_alive,
        "cleanup left the TERM-ignoring descendant alive"
    );
    assert!(!std::path::Path::new(&directory).exists());
    state.remove(&id);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn generated_supervisor_publishes_status_with_atomic_rename() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let prepare = observed::prepare_request(&target, &id, &directory);
    let prepared = Command::new("/bin/sh")
        .arg("-c")
        .arg(last_arg(&prepare, "read status-publish setup")?)
        .output()
        .map_err(|error| Error::io("prepare status-publish fixture", &error))?;
    assert!(prepared.status.success());

    let fake_bin = format!("/tmp/tinybox-observed-mv-{id}");
    std::fs::create_dir(&fake_bin)
        .map_err(|error| Error::io("create fake mv directory", &error))?;
    let mv_started = format!("/tmp/tinybox-observed-mv-started-{id}");
    let mv_release = format!("/tmp/tinybox-observed-mv-release-{id}");
    let mv_path = format!("{fake_bin}/mv");
    std::fs::write(
        &mv_path,
        "#!/bin/sh\nprintf 'ready\\n' > \"$TINYBOX_TEST_MV_STARTED\"\nwhile [ ! -f \"$TINYBOX_TEST_MV_RELEASE\" ]; do sleep 0.01; done\nexec /bin/mv \"$@\"\n",
    )
    .map_err(|error| Error::io("write fake mv command", &error))?;
    let mut permissions = std::fs::metadata(&mv_path)
        .map_err(|error| Error::io("inspect fake mv command", &error))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&mv_path, permissions)
        .map_err(|error| Error::io("make fake mv executable", &error))?;

    let launch = observed::request(
        &target,
        &ExecRequest::new(["/bin/sh", "-c", "exit 7"]),
        &id,
        &directory,
    )?;
    let launch_script = last_arg(&launch, "read status-race launcher")?;
    let mut wrapper = Command::new("/bin/sh")
        .arg("-c")
        .arg(launch_script)
        .env("PATH", format!("{fake_bin}:/usr/bin:/bin"))
        .env("TINYBOX_TEST_MV_STARTED", &mv_started)
        .env("TINYBOX_TEST_MV_RELEASE", &mv_release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::io("start status-publish supervisor", &error))?;

    let deadline = Instant::now() + Duration::from_secs(3);
    while !std::path::Path::new(&mv_started).exists() && Instant::now() < deadline {
        if wrapper
            .try_wait()
            .map_err(|error| Error::io("poll status-publish supervisor", &error))?
            .is_some()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let publisher_reached = std::path::Path::new(&mv_started).exists();
    let status_path = format!("{directory}/status");
    let status_tmp_path = format!("{directory}/status.tmp");
    let final_status_visible = std::path::Path::new(&status_path).exists();
    let temporary_status = std::fs::read_to_string(&status_tmp_path).ok();
    std::fs::write(&mv_release, "release\n")
        .map_err(|error| Error::io("release fake mv command", &error))?;
    let status = wrapper
        .wait()
        .map_err(|error| Error::io("wait for status-publish supervisor", &error))?;

    let _ = std::fs::remove_file(&mv_started);
    let _ = std::fs::remove_file(&mv_release);
    let _ = std::fs::remove_dir_all(&fake_bin);
    state.remove(&id);
    assert!(
        publisher_reached,
        "supervisor did not publish through the atomic rename gate"
    );
    assert!(
        !final_status_visible,
        "final status was visible before the complete temporary value was renamed"
    );
    assert_eq!(temporary_status.as_deref(), Some("7\n"));
    assert_eq!(status.code(), Some(7));
    assert!(!std::path::Path::new(&directory).exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn generated_supervisor_does_not_wait_for_a_lonely_group_anchor() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let prepare = observed::prepare_request(&target, &id, &directory);
    let prepared = Command::new("/bin/sh")
        .arg("-c")
        .arg(last_arg(&prepare, "read anchor-only setup")?)
        .output()
        .map_err(|error| Error::io("prepare anchor-only fixture", &error))?;
    assert!(prepared.status.success());

    let fake_bin = format!("{directory}/bin");
    std::fs::create_dir(&fake_bin)
        .map_err(|error| Error::io("create fake ps directory", &error))?;
    let ps_path = format!("{fake_bin}/ps");
    let ps_calls = format!("/tmp/tinybox-observed-ps-calls-{id}");
    std::fs::write(
        &ps_path,
        "#!/bin/sh\nprintf 'called\\n' >> \"$TINYBOX_TEST_PS_CALLS\"\ngroup=$(cat \"$TINYBOX_TEST_GROUP_FILE\") || exit 1\nprintf '%s %s\\n' \"$group\" \"$group\"\n",
    )
    .map_err(|error| Error::io("write fake ps command", &error))?;
    let mut permissions = std::fs::metadata(&ps_path)
        .map_err(|error| Error::io("inspect fake ps command", &error))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&ps_path, permissions)
        .map_err(|error| Error::io("make fake ps executable", &error))?;

    let launch = observed::request(
        &target,
        &ExecRequest::new(["/bin/sh", "-c", "exit 7"]),
        &id,
        &directory,
    )?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(last_arg(&launch, "read anchor-only launcher")?)
        .env("PATH", format!("{fake_bin}:/usr/bin:/bin"))
        .env("TINYBOX_TEST_PS_CALLS", &ps_calls)
        .env("TINYBOX_TEST_GROUP_FILE", format!("{directory}/pid"))
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::io("run anchor-only supervisor", &error))?;

    if !output.status.success() {
        let cleanup = observed::cleanup_request(&target, &id, &directory, false);
        if let Ok(script) = last_arg(&cleanup, "read anchor-only cleanup") {
            let _ = Command::new("/bin/sh").arg("-c").arg(script).status();
        }
    }
    state.remove(&id);
    assert_eq!(output.status.code(), Some(7));
    let calls = std::fs::read_to_string(&ps_calls)
        .map_err(|error| Error::io("read fake ps call log", &error))?;
    std::fs::remove_file(&ps_calls)
        .map_err(|error| Error::io("remove fake ps call log", &error))?;
    assert_eq!(
        calls.lines().count(),
        2,
        "the TERM grace loop should see that the anchor is the only remaining group member"
    );
    assert!(!std::path::Path::new(&directory).exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn generated_supervisor_keeps_grace_period_when_group_inspection_fails() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let prepare = observed::prepare_request(&target, &id, &directory);
    let prepared = Command::new("/bin/sh")
        .arg("-c")
        .arg(last_arg(&prepare, "read inspection-failure setup")?)
        .output()
        .map_err(|error| Error::io("prepare inspection-failure fixture", &error))?;
    assert!(prepared.status.success());

    let fake_bin = format!("{directory}/bin");
    std::fs::create_dir(&fake_bin)
        .map_err(|error| Error::io("create failed-inspection tool directory", &error))?;
    let ps_calls = format!("/tmp/tinybox-observed-failed-ps-{id}");
    let fake_ps = format!("{fake_bin}/ps");
    std::fs::write(
        &fake_ps,
        r#"#!/bin/sh
group=$(cat "$TINYBOX_TEST_GROUP_FILE") || exit 2
caller_group=$(awk '{print $5}' "/proc/$$/stat") || exit 2
if [ "$caller_group" = "$group" ]; then
  printf '%s\n' "$PPID" >> "$TINYBOX_TEST_PS_CALLS.child"
else
  printf '%s\n' "$PPID" >> "$TINYBOX_TEST_PS_CALLS"
fi
exit 1
"#,
    )
    .map_err(|error| Error::io("write failing ps command", &error))?;
    let fake_sleep = format!("{fake_bin}/sleep");
    std::fs::write(
        &fake_sleep,
        "#!/bin/sh\nif [ \"$1\" = 3600 ]; then exec /bin/sleep 3600; fi\nexit 0\n",
    )
    .map_err(|error| Error::io("write immediate test sleep command", &error))?;
    for path in [&fake_ps, &fake_sleep] {
        let mut permissions = std::fs::metadata(path)
            .map_err(|error| Error::io("inspect fake process command", &error))?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)
            .map_err(|error| Error::io("make fake process command executable", &error))?;
    }

    let launch = observed::request(
        &target,
        &ExecRequest::new(["/bin/sh", "-c", "exit 7"]),
        &id,
        &directory,
    )?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(last_arg(&launch, "read inspection-failure launcher")?)
        .env("PATH", format!("{fake_bin}:/usr/bin:/bin"))
        .env("TINYBOX_TEST_PS_CALLS", &ps_calls)
        .env("TINYBOX_TEST_GROUP_FILE", format!("{directory}/pid"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| Error::io("run inspection-failure supervisor", &error))?;

    let calls = std::fs::read_to_string(&ps_calls)
        .map_err(|error| Error::io("read failing ps call log", &error))?;
    let child_log = format!("{ps_calls}.child");
    let child_calls = match std::fs::read_to_string(&child_log) {
        Ok(calls) => calls,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(Error::io("read child process inspection log", &error)),
    };
    let _ = std::fs::remove_file(&ps_calls);
    let _ = std::fs::remove_file(&child_log);
    state.remove(&id);
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        calls.lines().count(),
        100,
        "failed process inspection must retain all supervisor TERM grace probes"
    );
    // The child can inspect its group once after recording status, unless TERM
    // reaches it first. That optional probe is separate from the supervisor grace.
    assert!(
        child_calls.lines().count() <= 1,
        "the child performs at most one process-group inspection before TERM"
    );
    assert!(!std::path::Path::new(&directory).exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn generated_supervisor_escalates_after_term_is_ignored_by_its_group() -> Result<()> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let prepare = observed::prepare_request(&target, &id, &directory);
    let prepare_script = last_arg(&prepare, "read TERM-ignore setup")?;
    let prepared = Command::new("/bin/sh")
        .arg("-c")
        .arg(prepare_script)
        .output()
        .map_err(|error| Error::io("reserve TERM-ignore fixture", &error))?;
    assert!(prepared.status.success());

    let launch = observed::request(
        &target,
        &ExecRequest::new(["/bin/sh", "-c", "trap '' TERM; sleep 600 & exit 7"]),
        &id,
        &directory,
    )?;
    let launch_script = last_arg(&launch, "read TERM-ignore launcher")?;
    let mut wrapper = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "trap '' TERM; exec /bin/sh -c {}",
            tinybox_core::shell::quote(launch_script)
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::io("start TERM-ignore supervisor", &error))?;

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exit_status = None;
    while Instant::now() < deadline {
        if let Some(status) = wrapper
            .try_wait()
            .map_err(|error| Error::io("poll TERM-ignore supervisor", &error))?
        {
            exit_status = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if exit_status.is_none() {
        if let Ok(group) = std::fs::read_to_string(format!("{directory}/pid")) {
            let _ = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("kill -KILL -{} 2>/dev/null || true", group.trim()))
                .status();
        }
        let _ = wrapper.kill();
        let _ = wrapper.wait();
        let cleanup = observed::cleanup_request(&target, &id, &directory, false);
        if let Ok(script) = last_arg(&cleanup, "read TERM-ignore recovery") {
            let _ = Command::new("/bin/sh").arg("-c").arg(script).status();
        }
    }
    assert!(
        exit_status.is_some(),
        "supervisor hung waiting for a TERM-ignoring process group before escalation"
    );
    assert_eq!(exit_status.and_then(|status| status.code()), Some(7));
    assert!(!std::path::Path::new(&directory).exists());
    state.remove(&id);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_keeps_a_reused_group_id_without_its_owner_marker() -> Result<()> {
    use std::process::{Command, Stdio};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    std::fs::create_dir(&directory)
        .map_err(|error| Error::io("create reused-group fixture", &error))?;
    std::fs::write(format!("{directory}/owner"), format!("{id}\n"))
        .map_err(|error| Error::io("write reused-group owner", &error))?;
    let mut exited = Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .map_err(|error| Error::io("start exited wrapper identity fixture", &error))?;
    let dead_wrapper = exited.id();
    exited
        .wait()
        .map_err(|error| Error::io("reap exited wrapper identity fixture", &error))?;
    std::fs::write(format!("{directory}/wrapper"), format!("{dead_wrapper}\n"))
        .map_err(|error| Error::io("write stale wrapper identity", &error))?;

    let mut unrelated = Command::new("setsid")
        .arg("sleep")
        .arg("600")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::io("start unrelated process group", &error))?;
    let group_output = Command::new("ps")
        .args(["-o", "pgid=", "-p", &unrelated.id().to_string()])
        .output()
        .map_err(|error| Error::io("inspect unrelated process group", &error))?;
    let unrelated_group = String::from_utf8_lossy(&group_output.stdout)
        .trim()
        .to_owned();
    assert_ne!(unrelated_group, "");
    let group_alive = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -0 -{unrelated_group} 2>/dev/null"))
        .status()
        .map_err(|error| Error::io("verify unrelated group exists", &error))?
        .success();
    assert!(group_alive, "fixture group {unrelated_group} must be live");
    std::fs::write(format!("{directory}/pid"), format!("{unrelated_group}\n"))
        .map_err(|error| Error::io("write reused group id", &error))?;

    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_script = last_arg(&cleanup, "read reused-group cleanup")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("run reused-group cleanup", &error))?;
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"pending\n");
    assert!(std::path::Path::new(&directory).exists());
    assert!(
        unrelated
            .try_wait()
            .map_err(|error| Error::io("check unrelated group", &error))?
            .is_none()
    );

    unrelated
        .kill()
        .map_err(|error| Error::io("stop unrelated process group", &error))?;
    unrelated
        .wait()
        .map_err(|error| Error::io("reap unrelated process group", &error))?;
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -KILL -{unrelated_group} 2>/dev/null || true"))
        .status();
    let cleanup = observed::cleanup_request(&target, &id, &directory, false);
    let cleanup_script = last_arg(&cleanup, "read reused-group retry cleanup")?;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cleanup_script)
        .output()
        .map_err(|error| Error::io("retry reused-group cleanup", &error))?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"clean\n");
    assert!(!std::path::Path::new(&directory).exists());
    state.remove(&id);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn a_live_foreground_command_has_no_fixed_completion_poll_limit() -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let target = SshTarget::new("builder")?;
    let state = observed::RemoteCleanupState::default();
    let (id, directory) = state.reserve();
    let prepare = observed::prepare_request(&target, &id, &directory);
    let prepare_script = last_arg(&prepare, "read long-command setup")?;
    let prepared = Command::new("/bin/sh")
        .arg("-c")
        .arg(prepare_script)
        .output()
        .map_err(|error| Error::io("reserve long-command fixture", &error))?;
    assert!(prepared.status.success());

    let poll_directory = format!("/tmp/.tinybox-observed-polls-{id}");
    std::fs::create_dir(&poll_directory)
        .map_err(|error| Error::io("create poll counter directory", &error))?;
    let poll_file = format!("{poll_directory}/count");
    let sleep_program = format!(
        "#!/bin/sh\nprintf x >> {}\n",
        tinybox_core::shell::quote(&poll_file)
    );
    let sleep_path = format!("{poll_directory}/sleep");
    std::fs::write(&sleep_path, sleep_program)
        .map_err(|error| Error::io("write poll counter command", &error))?;
    std::fs::set_permissions(&sleep_path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| Error::io("make poll counter executable", &error))?;

    let launch = observed::request(&target, &ExecRequest::new(["/bin/cat"]), &id, &directory)?;
    let launch_script = last_arg(&launch, "read long-command launcher")?;
    let mut wrapper = Command::new("/bin/sh")
        .arg("-c")
        .arg(launch_script)
        .env("PATH", format!("{poll_directory}:/usr/bin:/bin"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::io("start long-command wrapper", &error))?;

    let deadline = Instant::now() + Duration::from_secs(3);
    while std::fs::metadata(&poll_file).map_or(true, |metadata| metadata.len() < 1_100)
        && Instant::now() < deadline
    {
        if let Some(status) = wrapper
            .try_wait()
            .map_err(|error| Error::io("check long-command wrapper", &error))?
        {
            return Err(Error::Backend {
                sandbox: "ssh test".to_owned(),
                operation: "await long-command poll budget",
                message: format!("wrapper exited before controlled completion: {status}"),
            });
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let polls = std::fs::metadata(&poll_file)
        .map_err(|error| Error::io("read poll counter", &error))?
        .len();
    assert!(polls > 1_000, "observed {polls} completion polls");

    wrapper
        .stdin
        .take()
        .ok_or_else(|| fixture_error("take long-command stdin"))?
        .write_all(b"complete\n")
        .map_err(|error| Error::io("write long-command completion", &error))?;
    drop(wrapper.stdin.take());
    let status = wrapper
        .wait()
        .map_err(|error| Error::io("wait for long-command wrapper", &error))?;
    assert!(status.success());
    assert!(!std::path::Path::new(&directory).exists());
    std::fs::remove_file(&sleep_path)
        .map_err(|error| Error::io("remove poll counter command", &error))?;
    std::fs::remove_file(&poll_file).map_err(|error| Error::io("remove poll counter", &error))?;
    std::fs::remove_dir(&poll_directory)
        .map_err(|error| Error::io("remove poll counter directory", &error))?;
    state.remove(&id);
    Ok(())
}
