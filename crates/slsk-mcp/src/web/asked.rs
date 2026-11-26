//! Questions a page asks a peer or the server, owned here rather than by
//! the page or its stream. A question is asked once however many pages want
//! its answer at the same moment, and the answer, a refusal included, is
//! remembered for a while: pages open and close their streams constantly
//! (every minute, and on every move within a page), and none of that may
//! ask a peer again. An answer arriving announces [`Topic::Peers`], which
//! is how the streams that show it learn of it; they never ask themselves.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::live::Topic;

/// An answer, or why there is none.
pub(super) type Outcome<V> = Result<V, String>;

/// What is known of one question: in flight, or answered at a moment.
enum Known<V> {
    Asking,
    Answered(Instant, Outcome<V>),
}

/// The questions of one kind, by whom they are about.
pub(super) struct Asked<V> {
    known: parking_lot::Mutex<HashMap<String, Known<V>>>,
    /// How long an answer stands before the question may be asked again.
    keep: Duration,
    /// How long a refusal stands: shorter, as a peer may come back.
    keep_refused: Duration,
    /// The most answers remembered; the oldest go first.
    most: usize,
}

impl<V: Clone + Send + 'static> Asked<V> {
    pub(super) fn new(keep: Duration, keep_refused: Duration, most: usize) -> Self {
        Self {
            known: Default::default(),
            keep,
            keep_refused,
            most,
        }
    }

    fn fresh(&self, at: Instant, outcome: &Outcome<V>) -> bool {
        let keep = if outcome.is_ok() {
            self.keep
        } else {
            self.keep_refused
        };
        at.elapsed() < keep
    }

    /// The answer as it stands: `None` while it is asked, or when it was
    /// never asked or has lapsed.
    pub(super) fn answer(&self, key: &str) -> Option<Outcome<V>> {
        match self.known.lock().get(key)? {
            Known::Answered(at, outcome) if self.fresh(*at, outcome) => Some(outcome.clone()),
            _ => None,
        }
    }

    /// When the standing answer came, which changes only when a new one does:
    /// a stream draws an answer once, however many others land meanwhile.
    pub(super) fn answered_at(&self, key: &str) -> Option<Instant> {
        match self.known.lock().get(key)? {
            Known::Answered(at, outcome) if self.fresh(*at, outcome) => Some(*at),
            _ => None,
        }
    }

    /// Ask with `work`, unless the question is in flight or its answer
    /// stands.
    pub(super) fn ask<F>(self: &Arc<Self>, key: &str, work: F)
    where
        F: Future<Output = Outcome<V>> + Send + 'static,
    {
        {
            let mut known = self.known.lock();
            match known.get(key) {
                Some(Known::Asking) => return,
                Some(Known::Answered(at, outcome)) if self.fresh(*at, outcome) => return,
                _ => {}
            }
            known.insert(key.to_string(), Known::Asking);
        }
        let (asked, key) = (self.clone(), key.to_string());
        tokio::spawn(async move {
            let outcome = work.await;
            asked.settle(key, outcome);
        });
    }

    /// Forget the answer, so the next look asks again; one in flight stands.
    pub(super) fn forget(&self, key: &str) {
        let mut known = self.known.lock();
        if matches!(known.get(key), Some(Known::Answered(..))) {
            known.remove(key);
        }
    }

    fn settle(&self, key: String, outcome: Outcome<V>) {
        {
            let mut known = self.known.lock();
            known.retain(|_, k| match k {
                Known::Asking => true,
                Known::Answered(at, o) => self.fresh(*at, o),
            });
            while known.len() >= self.most {
                let oldest = known
                    .iter()
                    .filter_map(|(k, v)| match v {
                        Known::Answered(at, _) => Some((k.clone(), *at)),
                        Known::Asking => None,
                    })
                    .min_by_key(|(_, at)| *at);
                match oldest {
                    Some((k, _)) => {
                        known.remove(&k);
                    }
                    None => break,
                }
            }
            known.insert(key, Known::Answered(Instant::now(), outcome));
        }
        crate::live::changed(Topic::Peers);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::Asked;

    #[tokio::test]
    async fn a_question_is_asked_once_and_its_refusal_remembered() {
        let asked = Arc::new(Asked::<u32>::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            4,
        ));
        static TIMES: AtomicUsize = AtomicUsize::new(0);
        let work = || async {
            TIMES.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Err("offline".to_string())
        };
        asked.ask("ann", work());
        asked.ask("ann", work());
        assert_eq!(asked.answer("ann"), None);
        tokio::time::sleep(Duration::from_millis(100)).await;
        asked.ask("ann", work());
        assert_eq!(TIMES.load(Ordering::SeqCst), 1);
        assert_eq!(asked.answer("ann"), Some(Err("offline".into())));
        asked.forget("ann");
        assert_eq!(asked.answer("ann"), None);
    }

    #[tokio::test]
    async fn only_the_latest_answers_are_kept() {
        let asked = Arc::new(Asked::<u32>::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            2,
        ));
        for (n, user) in ["first", "second", "third"].into_iter().enumerate() {
            asked.ask(user, async move { Ok(n as u32) });
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(asked.answer("first"), None);
        assert_eq!(asked.answer("third"), Some(Ok(2)));
    }
}
