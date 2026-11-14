//! Same-process sync wakeups. Notify only after a write has committed.
//!
//! The database remains authoritative; watchers recheck their user/device snapshot
//! and retain periodic polling for writes from other processes.

use std::sync::LazyLock;

use tokio::sync::watch;

static UPDATES: LazyLock<watch::Sender<()>> = LazyLock::new(|| watch::channel(()).0);

pub fn subscribe() -> watch::Receiver<()> {
    UPDATES.subscribe()
}

pub fn notify() {
    UPDATES.send_replace(());
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn notifications_wake_all_subscribers_and_survive_until_waited_on() {
        let mut first = super::subscribe();
        let mut second = super::subscribe();
        super::notify();
        super::notify();
        for receiver in [&mut first, &mut second] {
            tokio::time::timeout(std::time::Duration::from_secs(1), receiver.changed())
                .await
                .unwrap()
                .unwrap();
        }
    }
}
