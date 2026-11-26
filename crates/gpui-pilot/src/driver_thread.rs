//! `DriverThread`: one long-lived thread owns the `Driver` and runs each tool call's work in turn.
//!
//! Launch forks the sandbox from whichever thread runs it, and bwrap's `--die-with-parent`
//! (`PR_SET_PDEATHSIG`) fires when that thread exits. Tokio retires an idle blocking-pool thread
//! after 10 s, so the work runs here instead, on a thread that lives as long as the server.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::thread::JoinHandle;

use tokio::sync::oneshot;

use crate::driver::Driver;

/// Why a job gave no answer.
#[derive(Debug)]
pub(crate) enum Failed {
    /// The work panicked; the driver thread caught it and carries on.
    Panicked(String),
    /// The driver thread is no longer taking work.
    Gone,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failed::Panicked(message) => write!(f, "panicked: {message}"),
            Failed::Gone => f.write_str("the driver thread has stopped"),
        }
    }
}

/// One queued tool call: it runs against the driver and sends its own answer.
type Job = Box<dyn FnOnce(&mut Driver) + Send>;

/// A handle that queues work on the driver thread; the thread ends once every handle is dropped.
#[derive(Clone)]
pub(crate) struct DriverThread {
    jobs: mpsc::Sender<Job>,
}

impl DriverThread {
    /// Starts the thread that owns `driver`; join the returned handle to wait for its teardown,
    /// which drops the driver (and its process group) on that same thread.
    pub fn start(driver: Driver) -> std::io::Result<(DriverThread, JoinHandle<()>)> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("gpui-pilot-driver".into())
            .spawn(move || {
                let mut driver = driver;
                for job in queue {
                    job(&mut driver);
                }
            })?;
        Ok((DriverThread { jobs }, thread))
    }

    /// Runs `work` against the driver after every job queued before it, and returns its result.
    /// A panic is caught inside the job, so the thread (and every child it forked) lives on; a
    /// caller that stops waiting leaves the job to finish and its answer to be dropped.
    pub async fn run<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Driver) -> R + Send + 'static,
    ) -> Result<R, Failed> {
        let (answer, answered) = oneshot::channel();
        let job: Job = Box::new(move |driver| {
            let outcome = catch_unwind(AssertUnwindSafe(|| work(driver)))
                .map_err(|payload| Failed::Panicked(panic_message(&*payload)));
            let _ = answer.send(outcome);
        });
        self.jobs.send(job).map_err(|_| Failed::Gone)?;
        answered.await.unwrap_or(Err(Failed::Gone))
    }
}

/// The text a `panic!` carried, when it was a string.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string payload".to_owned())
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command};
    use std::time::Duration;

    use super::*;
    use crate::launch::Timeouts;

    fn driver() -> Driver {
        let temp = std::env::temp_dir();
        Driver::new(temp.clone(), temp, Timeouts::default())
    }

    /// A runtime whose blocking pool retires idle threads at once, as tokio does after 10 s.
    fn impatient_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .thread_keep_alive(Duration::from_millis(10))
            .enable_all()
            .build()
            .unwrap()
    }

    /// `sleep 30` that the kernel kills when the thread that forked it exits, as bwrap's
    /// `--die-with-parent` arranges for the sandbox (util-linux `setpriv` sets the signal).
    fn sleep_tied_to_its_thread() -> Child {
        Command::new("setpriv")
            .args(["--pdeathsig", "KILL", "sleep", "30"])
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_child_forked_by_a_job_outlives_an_idle_pause() {
        let runtime = impatient_runtime();
        let (thread, joined) = DriverThread::start(driver()).unwrap();
        let mut child = runtime
            .block_on(thread.run(|_| sleep_tied_to_its_thread()))
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let exited = child.try_wait().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(exited.is_none(), "the child died while idle: {exited:?}");
        drop(thread);
        joined.join().unwrap();
    }

    #[test]
    fn a_panicking_job_fails_alone_and_the_next_runs_on_the_same_thread() {
        let runtime = impatient_runtime();
        let (thread, joined) = DriverThread::start(driver()).unwrap();
        let before = runtime
            .block_on(thread.run(|_| std::thread::current().id()))
            .unwrap();
        let failed = runtime
            .block_on(thread.run(|_| -> u8 { panic!("boom") }))
            .unwrap_err();
        assert_eq!(failed.to_string(), "panicked: boom");
        let after = runtime
            .block_on(thread.run(|driver| (std::thread::current().id(), driver.secrets().len())))
            .unwrap();
        assert_eq!(after, (before, 0));
        drop(thread);
        joined.join().unwrap();
    }

    #[test]
    fn the_thread_ends_once_every_handle_is_dropped() {
        let (thread, joined) = DriverThread::start(driver()).unwrap();
        let other = thread.clone();
        drop(thread);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!joined.is_finished(), "a live handle keeps the thread");
        drop(other);
        joined.join().unwrap();
    }
}
