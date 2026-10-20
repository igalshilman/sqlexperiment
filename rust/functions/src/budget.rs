//! A process-wide budget of the memory live runs hold: each run's VM and the
//! image beside it. A run that waits is committed, so it can let both go and
//! restore them from its scope when its next result arrives. Over the
//! budget, the runs that have waited longest park.
//!
//! A turn first reserves what it may take: its run's memory, or what turns of
//! its program took at their peak (the VM, the image and the commit's
//! buffers), if more. A turn that does not fit waits, while waiting runs
//! park, unless nothing else is live; turns that wait go first come, first
//! served.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

/// What a turn of a program with no turns yet may take: a VM of the most
/// memory it may have, and its image.
const UNKNOWN_PEAK: usize = 2 * crate::model::MAX_MEMORY;

/// Memory for live runs: `FUNCTIONS_MEMORY` (`512m`, `4g`), or no limit.
pub struct Budget {
    limit: Option<usize>,
    ledger: Mutex<Ledger>,
    /// Woken when memory is given back, for turns waiting to fit.
    freed: Notify,
}

#[derive(Default)]
struct Ledger {
    /// Bytes the leases hold.
    used: usize,
    /// Of them, those of runs told to park, not given back yet.
    parking: usize,
    next: u64,
    /// The runs waiting while live, the longest waiting first: the signal
    /// that tells each to park, and what it holds.
    waiting: BTreeMap<u64, (Arc<Notify>, usize)>,
    /// The peak of each program's recent turns.
    peaks: HashMap<String, usize>,
    /// The turns waiting to fit, in the order they came.
    queue: BTreeSet<u64>,
}

/// What one run holds against the budget, given back when it drops.
pub struct Lease {
    budget: Arc<Budget>,
    bytes: usize,
    /// While the run waits live: its place among the waiting, and what it
    /// held when it started to wait.
    waiting: Option<(u64, usize)>,
}

impl Budget {
    pub fn new(limit: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            limit,
            ledger: Mutex::default(),
            freed: Notify::new(),
        })
    }

    /// The budget `FUNCTIONS_MEMORY` sets.
    pub fn from_env() -> anyhow::Result<Arc<Self>> {
        let Ok(text) = std::env::var("FUNCTIONS_MEMORY") else {
            return Ok(Self::new(None));
        };
        let bytes = size(&text)
            .ok_or_else(|| anyhow::anyhow!("FUNCTIONS_MEMORY={text}: expected e.g. 512m, 4g"))?;
        Ok(Self::new(Some(bytes)))
    }

    pub fn lease(self: &Arc<Self>) -> Lease {
        Lease {
            budget: self.clone(),
            bytes: 0,
            waiting: None,
        }
    }

    /// Bytes held by live runs.
    pub fn used(&self) -> usize {
        self.ledger.lock().unwrap().used
    }

    /// What a turn of `program` may take at its peak.
    pub fn peak(&self, program: &str) -> usize {
        let ledger = self.ledger.lock().unwrap();
        ledger.peaks.get(program).copied().unwrap_or(UNKNOWN_PEAK)
    }

    /// A turn of `program` took `bytes` at its peak. The estimate follows a
    /// higher peak at once, and a lower one by an eighth each turn.
    pub fn record(&self, program: &str, bytes: usize) {
        let mut ledger = self.ledger.lock().unwrap();
        let peak = ledger.peaks.entry(program.to_owned()).or_insert(bytes);
        *peak = bytes.max(*peak - *peak / 8);
    }
}

impl Ledger {
    /// Tell the runs that have waited longest to park, until those parking
    /// hold `bytes`.
    fn park(&mut self, bytes: usize) {
        while self.parking < bytes {
            let Some((_, (park, held))) = self.waiting.pop_first() else {
                break;
            };
            park.notify_one();
            self.parking += held;
        }
    }
}

impl Lease {
    /// The run now holds `bytes`; past the budget, waiting runs park.
    pub fn hold(&mut self, bytes: usize) {
        let budget = &self.budget;
        let mut ledger = budget.ledger.lock().unwrap();
        ledger.used = ledger.used - self.bytes + bytes;
        let freed = bytes < self.bytes;
        self.bytes = bytes;
        if let Some(limit) = budget.limit {
            let over = ledger.used.saturating_sub(limit);
            ledger.park(over);
        }
        drop(ledger);
        if freed {
            budget.freed.notify_waiters();
        }
    }

    /// Hold `bytes` in all if they fit now, and no turn waits to fit first.
    pub fn try_reserve(&mut self, bytes: usize) -> bool {
        if let Some(limit) = self.budget.limit {
            let ledger = self.budget.ledger.lock().unwrap();
            let others = ledger.used - self.bytes;
            if !ledger.queue.is_empty() || others + bytes > limit && others > 0 {
                return false;
            }
        }
        self.hold(bytes);
        true
    }

    /// Wait until the run may hold `bytes` in all, then hold them. Turns
    /// that do not fit wait in turn; the first asks waiting runs to park. A
    /// run alone may hold any amount. A run should not wait here holding
    /// memory: nothing would ask it to give it back.
    pub async fn reserve(&mut self, bytes: usize) {
        let budget = self.budget.clone();
        if let Some(limit) = budget.limit {
            let mut queued: Option<Queued> = None;
            loop {
                let freed = budget.freed.notified();
                {
                    let mut ledger = budget.ledger.lock().unwrap();
                    let others = ledger.used - self.bytes;
                    let first = match &queued {
                        Some(queued) => ledger.queue.first() == Some(&queued.place),
                        None => ledger.queue.is_empty(),
                    };
                    if first && (others + bytes <= limit || others == 0) {
                        break;
                    }
                    if queued.is_none() {
                        let place = ledger.next;
                        ledger.next += 1;
                        ledger.queue.insert(place);
                        queued = Some(Queued {
                            budget: budget.clone(),
                            place,
                        });
                    }
                    if first {
                        ledger.park(others + bytes - limit);
                    }
                }
                freed.await;
            }
        }
        self.hold(bytes);
    }

    /// The run waits, live: the signal that tells it to park.
    pub fn wait(&mut self) -> Arc<Notify> {
        let park = Arc::new(Notify::new());
        let mut ledger = self.budget.ledger.lock().unwrap();
        let place = ledger.next;
        ledger.next += 1;
        ledger.waiting.insert(place, (park.clone(), self.bytes));
        self.waiting = Some((place, self.bytes));
        if let Some(limit) = self.budget.limit {
            let over = ledger.used.saturating_sub(limit);
            ledger.park(over);
        }
        park
    }

    /// The run's wait is over, whether or not it parked.
    pub fn woken(&mut self) {
        if let Some((place, held)) = self.waiting.take() {
            let mut ledger = self.budget.ledger.lock().unwrap();
            if ledger.waiting.remove(&place).is_none() {
                // It was told to park: it has, or will not now.
                ledger.parking -= held;
            }
        }
    }
}

/// A turn's place in the queue, left when it fits or gives up.
struct Queued {
    budget: Arc<Budget>,
    place: u64,
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.budget.ledger.lock().unwrap().queue.remove(&self.place);
        // The next in line may fit.
        self.budget.freed.notify_waiters();
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.woken();
        self.hold(0);
    }
}

/// `512m`, `4g`, or bytes.
fn size(text: &str) -> Option<usize> {
    let text = text.trim().to_ascii_lowercase();
    let (digits, shift) = match text.as_bytes().last()? {
        b'k' => (&text[..text.len() - 1], 10),
        b'm' => (&text[..text.len() - 1], 20),
        b'g' => (&text[..text.len() - 1], 30),
        _ => (text.as_str(), 0),
    };
    digits.parse::<usize>().ok()?.checked_mul(1 << shift)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sizes_read_as_written() {
        assert_eq!(size("512m"), Some(512 << 20));
        assert_eq!(size("4g"), Some(4 << 30));
        assert_eq!(size("64K"), Some(64 << 10));
        assert_eq!(size("1000"), Some(1000));
        for bad in ["", "m", "1.5g", "4gb"] {
            assert_eq!(size(bad), None, "{bad}");
        }
    }

    #[tokio::test]
    async fn the_longest_waiting_park_first() {
        let budget = Budget::new(Some(100));
        let (mut a, mut b, mut c) = (budget.lease(), budget.lease(), budget.lease());
        a.hold(40);
        let park_a = a.wait();
        b.hold(40);
        let park_b = b.wait();
        // 120 of 100: the longest waiting, a, parks, and only a.
        c.hold(40);
        tokio::time::timeout(Duration::from_secs(1), park_a.notified())
            .await
            .expect("a parks");
        a.hold(0);
        a.woken();
        assert_eq!(budget.used(), 80);
        let parked = tokio::time::timeout(Duration::from_millis(50), park_b.notified());
        assert!(parked.await.is_err(), "b keeps its memory");
    }

    #[tokio::test]
    async fn a_turn_waits_until_it_fits() {
        let budget = Budget::new(Some(100));
        let mut big = budget.lease();
        big.hold(90);
        let mut turn = budget.lease();
        {
            let wait = turn.reserve(30);
            tokio::pin!(wait);
            assert!(futures::poll!(wait.as_mut()).is_pending());
            big.hold(50);
            tokio::time::timeout(Duration::from_secs(1), wait)
                .await
                .expect("fits once memory is given back");
        }
        assert_eq!(budget.used(), 80);
    }

    #[tokio::test]
    async fn a_turn_parks_waiting_runs_to_fit() {
        let budget = Budget::new(Some(100));
        let mut idle = budget.lease();
        idle.hold(80);
        let park = idle.wait();
        let mut turn = budget.lease();
        let parked = async {
            park.notified().await;
            idle.hold(0);
            idle.woken();
        };
        let both = futures::future::join(turn.reserve(60), parked);
        tokio::time::timeout(Duration::from_secs(1), both)
            .await
            .expect("the waiting run parks, and the turn fits");
        assert_eq!(budget.used(), 60);
    }

    #[tokio::test]
    async fn waiting_turns_fit_in_the_order_they_came() {
        let budget = Budget::new(Some(100));
        let mut holder = budget.lease();
        holder.hold(100);
        let (mut big, mut small) = (budget.lease(), budget.lease());
        let big_turn = big.reserve(60);
        tokio::pin!(big_turn);
        assert!(futures::poll!(big_turn.as_mut()).is_pending());
        let small_turn = small.reserve(10);
        tokio::pin!(small_turn);
        assert!(futures::poll!(small_turn.as_mut()).is_pending());
        // Room for the small one, but the big one came first.
        holder.hold(80);
        tokio::task::yield_now().await;
        assert!(futures::poll!(small_turn.as_mut()).is_pending());
        holder.hold(30);
        tokio::time::timeout(Duration::from_secs(1), async {
            big_turn.await;
            small_turn.await;
        })
        .await
        .expect("both fit, in order");
        assert_eq!(budget.used(), 100);
    }

    #[tokio::test]
    async fn a_turn_that_gives_up_leaves_the_queue() {
        let budget = Budget::new(Some(100));
        let mut holder = budget.lease();
        holder.hold(100);
        let mut gone = budget.lease();
        {
            let turn = gone.reserve(50);
            tokio::pin!(turn);
            assert!(futures::poll!(turn.as_mut()).is_pending());
        }
        holder.hold(50);
        let mut next = budget.lease();
        tokio::time::timeout(Duration::from_secs(1), next.reserve(50))
            .await
            .expect("not behind the turn that gave up");
    }

    #[tokio::test]
    async fn no_turn_goes_ahead_of_one_waiting() {
        let budget = Budget::new(Some(100));
        let mut holder = budget.lease();
        holder.hold(90);
        let mut first = budget.lease();
        let waiting = first.reserve(50);
        tokio::pin!(waiting);
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        holder.hold(40);
        let mut late = budget.lease();
        assert!(!late.try_reserve(5), "the first is still in line");
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the first fits");
        assert!(late.try_reserve(5));
        assert_eq!(budget.used(), 95);
    }

    #[tokio::test]
    async fn a_run_alone_may_exceed_the_budget() {
        let budget = Budget::new(Some(100));
        let mut alone = budget.lease();
        tokio::time::timeout(Duration::from_secs(1), alone.reserve(500))
            .await
            .expect("nothing else to wait for");
    }

    #[test]
    fn peaks_rise_at_once_and_fall_slowly() {
        let budget = Budget::new(Some(100));
        assert_eq!(budget.peak("p"), UNKNOWN_PEAK);
        budget.record("p", 800);
        budget.record("p", 100);
        assert_eq!(budget.peak("p"), 700);
        budget.record("p", 900);
        assert_eq!(budget.peak("p"), 900);
    }
}
