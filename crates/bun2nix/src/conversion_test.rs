use std::sync::{Barrier, Mutex};

use super::{Prefetcher, prefetch_bounded};
use crate::{Error, Result};

struct ConcurrentPrefetcher {
    barrier: Barrier,
    calls: Mutex<Vec<String>>,
}

impl Prefetcher for ConcurrentPrefetcher {
    fn prefetch(&self, source: &str) -> Result<String> {
        self.calls.lock().unwrap().push(source.to_owned());
        self.barrier.wait();
        Ok(format!("hash-{source}"))
    }
}

#[test]
fn bounded_prefetch_runs_batches_concurrently_and_preserves_order() {
    let prefetcher = ConcurrentPrefetcher {
        barrier: Barrier::new(2),
        calls: Mutex::default(),
    };
    let sources = ["a", "b", "c", "d"].map(str::to_owned);
    assert_eq!(
        prefetch_bounded(&prefetcher, &sources, 2).unwrap(),
        ["hash-a", "hash-b", "hash-c", "hash-d"]
    );
    let calls = prefetcher.calls.lock().unwrap();
    assert!(
        calls[..2]
            .iter()
            .all(|source| source == "a" || source == "b")
    );
    assert!(
        calls[2..]
            .iter()
            .all(|source| source == "c" || source == "d")
    );
}

#[test]
fn bounded_prefetch_stops_before_next_batch_after_failure() {
    struct Failing(Mutex<Vec<String>>);
    impl Prefetcher for Failing {
        fn prefetch(&self, source: &str) -> Result<String> {
            self.0.lock().unwrap().push(source.to_owned());
            Err(Error::PrefetchFailed {
                locator: source.to_owned(),
                stderr: "failed".to_owned(),
            })
        }
    }
    let prefetcher = Failing(Mutex::default());
    let sources = ["a", "b", "c"].map(str::to_owned);
    assert!(
        matches!(prefetch_bounded(&prefetcher, &sources, 2), Err(Error::PrefetchFailed { locator, .. }) if locator == "a")
    );
    assert_eq!(prefetcher.0.lock().unwrap().len(), 2);
}
