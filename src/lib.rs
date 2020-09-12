//! Set timeouts on [os_clock](https://crates.io/crates/os_clock)s.
//!
//! Depends on having a multi-threaded tokio runtime.
//!
//! ## Example:
//!
//! ```
//! use std::time::Duration;
//! use std::sync::Arc;
//! use std::sync::atomic::{AtomicUsize, Ordering};
//! use os_clock::ThreadCPUClock;
//! use tokio;
//!
//! #[tokio::main]
//! async fn main() {
//!     // must be called inside a Tokio runtime.
//!     let timer = timeouts::Timer::<ThreadCPUClock>::spawn();
//!
//!     let thread_handle = std::thread::spawn(move || {
//!         let spinlock = Arc::new(AtomicUsize::new(1));
//!         let spinlock_clone = spinlock.clone();
//!
//!         let timeout = &timer.set_timeout_on_current_thread_cpu_usage(Duration::from_millis(2), move || {
//!             spinlock_clone.store(0, Ordering::Relaxed);
//!         });
//!
//!         let mut i = 0;
//!
//!         while spinlock.load(Ordering::Relaxed) != 0 {
//!           i += 1;
//!         }
//!
//!         println!("Num iters: {:?}", i);
//!     });
//!
//!     thread_handle.join();
//! }
//! ```

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::delay_for;

use os_clock::{self, Clock, ThreadCPUClock};

use futures::executor::block_on;

async fn delay_until_clock<C: Clock>(clock: C, timeout_at: Duration) -> std::io::Result<()> {
    loop {
        let current = clock.get_time()?;

        if current >= timeout_at {
            return Ok(());
        }

        delay_for(timeout_at - current).await;
    }
}

/// Called on timeout
pub struct TimeoutListener(Box<dyn (FnOnce()) + Send>);

impl<L: (FnOnce()) + Send> From<L> for TimeoutListener
where
    L: 'static,
{
    fn from(listener: L) -> TimeoutListener {
        TimeoutListener(Box::new(listener))
    }
}

struct TimeoutRequest<C: Clock> {
    cancel_rx: oneshot::Receiver<()>,
    result_tx: oneshot::Sender<TimeoutResult>,
    on_timeout: TimeoutListener,
    clock: C,
    timeout_at: Duration,
}

impl<C: Clock> TimeoutRequest<C> {
    async fn run(self) {
        let on_timeout = self.on_timeout;
        let cancel_rx = self.cancel_rx;
        let result = tokio::select! {
          Ok(_) = cancel_rx => {
            TimeoutResult::Cancelled
          },
          result = delay_until_clock(self.clock, self.timeout_at) => {
            match result {
              Ok(_) => {
                on_timeout.0();
                TimeoutResult::TimedOut
              },
              Err(err) => TimeoutResult::StatusError(err)
            }
          }
        };

        // ignored because failure to send just means that no one cares about the result
        let _ = self.result_tx.send(result);
    }
}

pub enum TimeoutResult {
    /// Timed out, the on_timeout handler was called
    TimedOut,
    /// The timeout was cancelled, the on_timeout handler was not called
    Cancelled,
    /// A status error occurred, the on_timeout handler was not called
    ///
    /// Likely due to the thread being measured finishing before timing out
    StatusError(std::io::Error),
}

pub struct TimeoutHandle {
    cancel_tx: oneshot::Sender<()>,
    result_rx: oneshot::Receiver<TimeoutResult>,
}

impl TimeoutHandle {
    /// Cancel a timeout. Returns immediately, sometimes prevents the timeout listener from running
    ///
    /// Note we'll return immediately, prior to cancelling the thread
    pub fn cancel_ignored(self) {
        let _ = self.cancel_tx.send(());
    }

    /// Cancel a timeout if not already timed out and wait for the result
    pub async fn cancel(self) -> TimeoutResult {
        let _ = self.cancel_tx.send(());

        self.result_rx.await.unwrap()
    }

    /// Cancel a timeout if not already timed out and wait for the result
    pub fn cancel_sync(self) -> TimeoutResult {
        block_on(self.cancel())
    }
}

pub struct Timer<C: Clock>
where
    C: 'static,
{
    tx: mpsc::UnboundedSender<TimeoutRequest<C>>,
}

impl<C: Clock> Timer<C> {
    /// Set a timeout. Will execute the callback if not cancelled before executing
    pub fn set_timeout<T: Into<TimeoutListener>>(
        &self,
        clock: C,
        duration: Duration,
        callback: T,
    ) -> TimeoutHandle {
        let timeout_at = clock.get_time().unwrap() + duration;

        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();

        self.tx
            .send(TimeoutRequest {
                cancel_rx,
                result_tx,
                timeout_at,
                on_timeout: callback.into(),
                clock,
            })
            .map_err(|_| {})
            .unwrap(); // this will never fail with a correctly instituted timeout scheduler

        return TimeoutHandle {
            cancel_tx,
            result_rx,
        };
    }

    /// Spawn a new timer
    pub fn spawn() -> Self {
        let (timeout_req_tx, mut timeout_req_rx) = mpsc::unbounded_channel::<TimeoutRequest<C>>();

        tokio::spawn(async move {
            while let Some(timeout) = timeout_req_rx.recv().await {
                tokio::spawn(timeout.run());
            }
        });

        return Timer { tx: timeout_req_tx };
    }
}

impl Timer<ThreadCPUClock> {
    pub fn set_timeout_on_current_thread_cpu_usage<T: Into<TimeoutListener>>(
        &self,
        duration: Duration,
        callback: T,
    ) -> TimeoutHandle {
        let clock = os_clock::cpu_clock_for_current_thread().unwrap();

        self.set_timeout(clock, duration, callback)
    }
}
