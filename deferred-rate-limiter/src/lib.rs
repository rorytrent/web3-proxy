//#![warn(missing_docs)]
use moka::future::Cache;
use redis_rate_limiter::{RedisRateLimitResult, RedisRateLimiter};
use std::cmp::Eq;
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{atomic::AtomicU64, Arc};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::error;

/// A local cache that sits in front of a RedisRateLimiter
/// Generic accross the key so it is simple to use with IPs or user keys
pub struct DeferredRateLimiter<K>
where
    K: Send + Sync,
{
    local_cache: Cache<K, Arc<AtomicU64>, ahash::RandomState>,
    prefix: String,
    rrl: RedisRateLimiter,
}

pub enum DeferredRateLimitResult {
    Allowed,
    RetryAt(Instant),
    RetryNever,
}

impl<K> DeferredRateLimiter<K>
where
    K: Copy + Debug + Display + Hash + Eq + Send + Sync + 'static,
{
    pub fn new(cache_size: u64, prefix: &str, rrl: RedisRateLimiter) -> Self {
        let ttl = rrl.period as u64;

        // TODO: time to live is not right. we want this ttl counter to start only after redis is down. this works for now
        let local_cache = Cache::builder()
            .time_to_live(Duration::from_secs(ttl))
            .max_capacity(cache_size)
            .name(prefix)
            .build_with_hasher(ahash::RandomState::new());

        Self {
            local_cache,
            prefix: prefix.to_string(),
            rrl,
        }
    }

    /// if setting max_per_period, be sure to keep the period the same for all requests to this label
    pub async fn throttle(
        &self,
        key: &K,
        max_per_period: Option<u64>,
        count: u64,
    ) -> anyhow::Result<DeferredRateLimitResult> {
        let max_per_period = max_per_period.unwrap_or(self.rrl.max_requests_per_period);

        if max_per_period == 0 {
            return Ok(DeferredRateLimitResult::RetryNever);
        }

        let arc_new_entry = Arc::new(AtomicBool::new(false));

        // TODO: this can't be right. what type do we actually want here?
        let arc_retry_at = Arc::new(Mutex::new(None));

        let redis_key = format!("{}:{}", self.prefix, key);

        // TODO: DO NOT UNWRAP HERE. figure out how to handle anyhow error being wrapped in an Arc
        // TODO: i'm sure this could be a lot better. but race conditions make this hard to think through. brain needs sleep
        let arc_key_count: Arc<AtomicU64> = {
            // clone things outside of the
            let arc_new_entry = arc_new_entry.clone();
            let arc_retry_at = arc_retry_at.clone();
            let redis_key = redis_key.clone();
            let rrl = Arc::new(self.rrl.clone());

            self.local_cache
                .get_with(*key, async move {
                    arc_new_entry.store(true, Ordering::Release);

                    // we do not use the try operator here because we want to be okay with redis errors
                    let redis_count = match rrl
                        .throttle_label(&redis_key, Some(max_per_period), count)
                        .await
                    {
                        Ok(RedisRateLimitResult::Allowed(count)) => count,
                        Ok(RedisRateLimitResult::RetryAt(retry_at, count)) => {
                            let _ = arc_retry_at.lock().await.insert(Some(retry_at));
                            count
                        }
                        Ok(RedisRateLimitResult::RetryNever) => unimplemented!(),
                        Err(err) => {
                            // if we get a redis error, just let the user through. local caches will work fine
                            // though now that we do this, we need to reset rate limits every minute!
                            error!(?err, "unable to rate limit! creating empty cache");
                            0
                        }
                    };

                    Arc::new(AtomicU64::new(redis_count))
                })
                .await
        };

        if arc_new_entry.load(Ordering::Acquire) {
            // new entry. redis was already incremented
            // return the retry_at that we got from
            if let Some(Some(retry_at)) = arc_retry_at.lock().await.take() {
                Ok(DeferredRateLimitResult::RetryAt(retry_at))
            } else {
                Ok(DeferredRateLimitResult::Allowed)
            }
        } else {
            // we have a cached amount here
            let cached_key_count = arc_key_count.fetch_add(count, Ordering::Acquire);

            // assuming no other parallel futures incremented this key, this is the count that redis has
            let expected_key_count = cached_key_count + count;

            if expected_key_count > max_per_period {
                // rate limit overshot!
                let now = self.rrl.now_as_secs();

                // do not fetch_sub
                // another row might have queued a redis throttle_label to keep our count accurate

                // show that we are rate limited without even querying redis
                let retry_at = self.rrl.next_period(now);
                Ok(DeferredRateLimitResult::RetryAt(retry_at))
            } else {
                // local caches think rate limit should be okay

                // prepare a future to update redis
                let rate_limit_f = {
                    let rrl = self.rrl.clone();
                    async move {
                        match rrl
                            .throttle_label(&redis_key, Some(max_per_period), count)
                            .await
                        {
                            Ok(RedisRateLimitResult::Allowed(count)) => {
                                arc_key_count.store(count, Ordering::Release);
                                DeferredRateLimitResult::Allowed
                            }
                            Ok(RedisRateLimitResult::RetryAt(retry_at, count)) => {
                                arc_key_count.store(count, Ordering::Release);
                                DeferredRateLimitResult::RetryAt(retry_at)
                            }
                            Ok(RedisRateLimitResult::RetryNever) => {
                                // TODO: what should we do to arc_key_count?
                                DeferredRateLimitResult::RetryNever
                            }
                            Err(err) => {
                                // don't let redis errors block our users!
                                error!(
                                    // ?key,  // TODO: this errors
                                    ?err,
                                    "unable to query rate limits. local cache available"
                                );
                                // TODO: we need to start a timer that resets this count every minute
                                DeferredRateLimitResult::Allowed
                            }
                        }
                    }
                };

                // if close to max_per_period, wait for redis
                // TODO: how close should we allow? depends on max expected concurent requests from one user
                if expected_key_count > max_per_period * 99 / 100 {
                    // close to period. don't risk it. wait on redis
                    Ok(rate_limit_f.await)
                } else {
                    // rate limit has enough headroom that it should be safe to do this in the background
                    tokio::spawn(rate_limit_f);

                    Ok(DeferredRateLimitResult::Allowed)
                }
            }
        }
    }
}
