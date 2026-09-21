use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::http::HeaderValue;
use tokio::sync::oneshot;

use super::{GasPriceError, GasPriceQuote};

const MAX_CACHE_ENTRIES: usize = 1_024;

type QuoteResult = Result<GasPriceQuote, GasPriceError>;

#[derive(Clone)]
pub struct GasPriceCache {
    ttl: Duration,
    max_entries: usize,
    state: Arc<Mutex<CacheState>>,
}

pub enum CacheRequest {
    Hit(GasPriceQuote),
    Leader(CacheLeader),
    Follower(oneshot::Receiver<QuoteResult>),
}

pub struct CacheLeader {
    cache: GasPriceCache,
    key: CacheKey,
    completed: bool,
}

struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    in_flight: HashMap<CacheKey, Vec<oneshot::Sender<QuoteResult>>>,
}

struct CacheEntry {
    quote: GasPriceQuote,
    expires_at: Instant,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct CacheKey {
    chain_id: u64,
    user_rpc_hash: u64,
}

impl GasPriceCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            max_entries: MAX_CACHE_ENTRIES,
            state: Arc::new(Mutex::new(CacheState {
                entries: HashMap::new(),
                in_flight: HashMap::new(),
            })),
        }
    }

    pub fn request(&self, chain_id: u64, user_rpc_url: Option<&HeaderValue>) -> CacheRequest {
        let key = CacheKey::new(chain_id, user_rpc_url);
        let now = Instant::now();
        let mut state = self.lock_state();
        state.entries.retain(|_, entry| entry.expires_at > now);

        if let Some(entry) = state.entries.get(&key) {
            return CacheRequest::Hit(entry.quote.clone());
        }

        if let Some(waiters) = state.in_flight.get_mut(&key) {
            let (sender, receiver) = oneshot::channel();
            waiters.push(sender);
            return CacheRequest::Follower(receiver);
        }

        state.in_flight.insert(key.clone(), Vec::new());
        CacheRequest::Leader(CacheLeader {
            cache: self.clone(),
            key,
            completed: false,
        })
    }

    fn complete(&self, key: &CacheKey, result: QuoteResult) {
        let now = Instant::now();
        let waiters = {
            let mut state = self.lock_state();
            state.entries.retain(|_, entry| entry.expires_at > now);

            if let Ok(quote) = &result
                && (state.entries.contains_key(key) || state.entries.len() < self.max_entries)
            {
                state.entries.insert(
                    key.clone(),
                    CacheEntry {
                        quote: quote.clone(),
                        expires_at: now + self.ttl,
                    },
                );
            }

            state.in_flight.remove(key).unwrap_or_default()
        };

        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl CacheLeader {
    pub fn complete(mut self, result: QuoteResult) {
        self.cache.complete(&self.key, result);
        self.completed = true;
    }
}

impl Drop for CacheLeader {
    fn drop(&mut self) {
        if !self.completed {
            self.cache
                .complete(&self.key, Err(GasPriceError::NoPriceAvailable));
        }
    }
}

impl CacheKey {
    fn new(chain_id: u64, user_rpc_url: Option<&HeaderValue>) -> Self {
        let mut hasher = DefaultHasher::new();
        user_rpc_url.is_some().hash(&mut hasher);
        if let Some(user_rpc_url) = user_rpc_url {
            user_rpc_url.as_bytes().hash(&mut hasher);
        }

        Self {
            chain_id,
            user_rpc_hash: hasher.finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::http::HeaderValue;
    use tokio::sync::Barrier;

    use super::{CacheRequest, GasPriceCache};
    use vela_relay_core::gas_math::{NetworkGasPrice, tiers};

    use crate::gas_price::GasPriceQuote;

    fn quote() -> GasPriceQuote {
        GasPriceQuote {
            tiers: tiers(NetworkGasPrice {
                base_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
            })
            .unwrap(),
            rpc_domain: "rpc.example.com".into(),
        }
    }

    #[test]
    fn returns_a_successful_quote_until_its_ttl_expires() {
        let cache = GasPriceCache::new(Duration::from_secs(5));
        let quote = quote();
        let CacheRequest::Leader(leader) = cache.request(1, None) else {
            panic!("the first request must lead the refresh");
        };
        leader.complete(Ok(quote.clone()));

        let CacheRequest::Hit(cached_quote) = cache.request(1, None) else {
            panic!("the second request must use the cached quote");
        };
        assert_eq!(cached_quote, quote);
    }

    #[tokio::test]
    async fn coalesces_concurrent_requests_for_the_same_price() {
        let cache = GasPriceCache::new(Duration::from_secs(5));
        let quote = quote();
        let CacheRequest::Leader(leader) = cache.request(1, None) else {
            panic!("the first request must lead the refresh");
        };
        let CacheRequest::Follower(waiter) = cache.request(1, None) else {
            panic!("a concurrent request must wait for the refresh");
        };

        leader.complete(Ok(quote.clone()));

        assert_eq!(waiter.await.unwrap(), Ok(quote));
    }

    #[tokio::test]
    async fn performs_one_upstream_calculation_for_sixty_four_concurrent_misses() {
        let cache = GasPriceCache::new(Duration::from_secs(5));
        let quote = quote();
        let upstream_calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(64));

        let tasks = (0..64)
            .map(|_| {
                let cache = cache.clone();
                let quote = quote.clone();
                let upstream_calls = Arc::clone(&upstream_calls);
                let barrier = Arc::clone(&barrier);

                tokio::spawn(async move {
                    barrier.wait().await;

                    match cache.request(1, None) {
                        CacheRequest::Hit(quote) => Ok(quote),
                        CacheRequest::Follower(waiter) => waiter
                            .await
                            .unwrap_or(Err(crate::gas_price::GasPriceError::NoPriceAvailable)),
                        CacheRequest::Leader(leader) => {
                            upstream_calls.fetch_add(1, Ordering::Relaxed);
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            leader.complete(Ok(quote.clone()));
                            Ok(quote)
                        }
                    }
                })
            })
            .collect::<Vec<_>>();

        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), quote);
        }
        assert_eq!(upstream_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn separates_callers_that_configure_different_rpc_urls() {
        let cache = GasPriceCache::new(Duration::from_secs(5));
        let first_rpc = HeaderValue::from_static("https://first.example.com");
        let second_rpc = HeaderValue::from_static("https://second.example.com");

        assert!(matches!(
            cache.request(1, Some(&first_rpc)),
            CacheRequest::Leader(_)
        ));
        assert!(matches!(
            cache.request(1, Some(&second_rpc)),
            CacheRequest::Leader(_)
        ));
    }
}
