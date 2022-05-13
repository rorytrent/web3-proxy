use crate::config::Web3ConnectionConfig;
use crate::connections::Web3Connections;
use crate::jsonrpc::JsonRpcErrorData;
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcForwardedResponseEnum;
use crate::jsonrpc::JsonRpcRequest;
use crate::jsonrpc::JsonRpcRequestEnum;
use ethers::prelude::ProviderError;
use ethers::prelude::{HttpClientError, WsClientError};
use futures::future;
use futures::future::join_all;
use governor::clock::{Clock, QuantaClock};
use linkedhashmap::LinkedHashMap;
use std::fmt;
use std::sync::atomic::{self, AtomicU64};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{trace, warn};

static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

// TODO: put this in config? what size should we do?
const RESPONSE_CACHE_CAP: usize = 1024;

/// TODO: these types are probably very bad keys and values. i couldn't get caching of warp::reply::Json to work
type ResponseLruCache =
    RwLock<LinkedHashMap<(u64, String, Option<String>), JsonRpcForwardedResponse>>;

/// The application
// TODO: this debug impl is way too verbose. make something smaller
// TODO: if Web3ProxyApp is always in an Arc, i think we can avoid having at least some of these internal things in arcs
pub struct Web3ProxyApp {
    best_head_block_number: Arc<AtomicU64>,
    /// clock used for rate limiting
    /// TODO: use tokio's clock (will require a different ratelimiting crate)
    clock: QuantaClock,
    /// Send requests to the best server available
    balanced_rpc_tiers: Vec<Arc<Web3Connections>>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    private_rpcs: Option<Arc<Web3Connections>>,
    response_cache: ResponseLruCache,
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp")
            .field(
                "best_head_block_number",
                &self.best_head_block_number.load(atomic::Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl Web3ProxyApp {
    pub async fn try_new(
        chain_id: usize,
        balanced_rpc_tiers: Vec<Vec<Web3ConnectionConfig>>,
        private_rpcs: Vec<Web3ConnectionConfig>,
    ) -> anyhow::Result<Web3ProxyApp> {
        let clock = QuantaClock::default();

        let best_head_block_number = Arc::new(AtomicU64::new(0));

        // make a http shared client
        // TODO: how should we configure the connection pool?
        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something is wrong with the remote server
        let http_client = reqwest::ClientBuilder::new()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(300))
            .user_agent(APP_USER_AGENT)
            .build()?;

        // TODO: attach context to this error
        let balanced_rpc_tiers =
            future::join_all(balanced_rpc_tiers.into_iter().map(|balanced_rpc_tier| {
                Web3Connections::try_new(
                    chain_id,
                    best_head_block_number.clone(),
                    balanced_rpc_tier,
                    Some(http_client.clone()),
                    &clock,
                    true,
                )
            }))
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<Arc<Web3Connections>>>>()?;

        // TODO: attach context to this error
        let private_rpcs = if private_rpcs.is_empty() {
            warn!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            // TODO: instead of None, set it to a list of all the rpcs from balanced_rpc_tiers. that way we broadcast very loudly
            None
        } else {
            Some(
                Web3Connections::try_new(
                    chain_id,
                    best_head_block_number.clone(),
                    private_rpcs,
                    Some(http_client),
                    &clock,
                    false,
                )
                .await?,
            )
        };

        Ok(Web3ProxyApp {
            best_head_block_number,
            clock,
            balanced_rpc_tiers,
            private_rpcs,
            response_cache: Default::default(),
        })
    }

    /// send the request to the approriate RPCs
    /// TODO: dry this up
    pub async fn proxy_web3_rpc(
        self: Arc<Web3ProxyApp>,
        request: JsonRpcRequestEnum,
    ) -> anyhow::Result<impl warp::Reply> {
        trace!("Received request: {:?}", request);

        let response = match request {
            JsonRpcRequestEnum::Single(request) => {
                JsonRpcForwardedResponseEnum::Single(self.proxy_web3_rpc_request(request).await?)
            }
            JsonRpcRequestEnum::Batch(requests) => {
                JsonRpcForwardedResponseEnum::Batch(self.proxy_web3_rpc_requests(requests).await?)
            }
        };

        Ok(warp::reply::json(&response))
    }

    async fn proxy_web3_rpc_requests(
        self: Arc<Web3ProxyApp>,
        requests: Vec<JsonRpcRequest>,
    ) -> anyhow::Result<Vec<JsonRpcForwardedResponse>> {
        // TODO: we should probably change ethers-rs to support this directly
        // we cut up the request and send to potentually different servers. this could be a problem.
        // if the client needs consistent blocks, they should specify instead of assume batches work on the same
        // TODO: is spawning here actually slower?
        let num_requests = requests.len();
        let responses = join_all(
            requests
                .into_iter()
                .map(|request| {
                    let clone = self.clone();
                    tokio::spawn(async move { clone.proxy_web3_rpc_request(request).await })
                })
                .collect::<Vec<_>>(),
        )
        .await;

        // TODO: i'm sure this could be done better with iterators
        let mut collected: Vec<JsonRpcForwardedResponse> = Vec::with_capacity(num_requests);
        for response in responses {
            collected.push(response??);
        }

        Ok(collected)
    }

    async fn proxy_web3_rpc_request(
        self: Arc<Web3ProxyApp>,
        request: JsonRpcRequest,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        trace!("Received request: {:?}", request);

        // TODO: apparently json_body can be a vec of multiple requests. should we split them up?  we need to respond with a Vec too

        if self.private_rpcs.is_some() && request.method == "eth_sendRawTransaction" {
            let private_rpcs = self.private_rpcs.as_ref().unwrap();

            // there are private rpcs configured and the request is eth_sendSignedTransaction. send to all private rpcs
            loop {
                // TODO: think more about this lock. i think it won't actually help the herd. it probably makes it worse if we have a tight lag_limit
                match private_rpcs.get_upstream_servers() {
                    Ok(active_request_handles) => {
                        let (tx, rx) = flume::unbounded();

                        let connections = private_rpcs.clone();
                        let method = request.method.clone();
                        let params = request.params.clone();

                        // TODO: benchmark this compared to waiting on unbounded futures
                        // TODO: do something with this handle?
                        tokio::spawn(async move {
                            connections
                                .try_send_parallel_requests(
                                    active_request_handles,
                                    method,
                                    params,
                                    tx,
                                )
                                .await
                        });

                        // wait for the first response
                        let backend_response = rx.recv_async().await?;

                        if let Ok(backend_response) = backend_response {
                            // TODO: i think we
                            let response = JsonRpcForwardedResponse {
                                jsonrpc: "2.0".to_string(),
                                id: request.id,
                                result: Some(backend_response),
                                error: None,
                            };
                            return Ok(response);
                        }
                    }
                    Err(not_until) => {
                        // TODO: move this to a helper function
                        // sleep (TODO: with a lock?) until our rate limits should be available
                        // TODO: if a server catches up sync while we are waiting, we could stop waiting
                        if let Some(not_until) = not_until {
                            let deadline = not_until.wait_time_from(self.clock.now());

                            sleep(deadline).await;
                        } else {
                            // TODO: what should we do here?
                            return Err(anyhow::anyhow!("no private rpcs!"));
                        }
                    }
                };
            }
        } else {
            // this is not a private transaction (or no private relays are configured)
            // try to send to each tier, stopping at the first success
            // if no tiers are synced, fallback to privates
            loop {
                // there are multiple tiers. save the earliest not_until (if any). if we don't return, we will sleep until then and then try again
                let mut earliest_not_until = None;

                // TODO: how can we better build this iterator?
                let rpc_iter = if let Some(private_rpcs) = self.private_rpcs.as_ref() {
                    self.balanced_rpc_tiers.iter().chain(vec![private_rpcs])
                } else {
                    self.balanced_rpc_tiers.iter().chain(vec![])
                };

                for balanced_rpcs in rpc_iter {
                    let best_head_block_number =
                        self.best_head_block_number.load(atomic::Ordering::Acquire);

                    let best_rpc_block_number = balanced_rpcs.head_block_number();

                    if best_rpc_block_number < best_head_block_number {
                        continue;
                    }

                    // TODO: building this cache key is slow and its large, but i don't see a better way right now
                    // TODO: inspect the params and see if a block is specified. if so, use that block number instead of current_block
                    // TODO: move this to a helper function. have it be a lot smarter
                    // TODO: have 2 caches. a small `block_cache` for most calls and a large `immutable_cache` for items that won't change
                    let cache_key = (
                        best_head_block_number,
                        request.method.clone(),
                        request.params.clone().map(|x| x.to_string()),
                    );

                    if let Some(cached) = self.response_cache.read().await.get(&cache_key) {
                        // TODO: this still serializes every time
                        // TODO: return a reference in the other places so that this works without a clone?
                        return Ok(cached.to_owned());
                    }

                    // TODO: what allowed lag?
                    match balanced_rpcs.next_upstream_server().await {
                        Ok(active_request_handle) => {
                            let response = active_request_handle
                                .request(&request.method, &request.params)
                                .await;

                            let response = match response {
                                Ok(partial_response) => {
                                    // TODO: trace here was really slow with millions of requests.
                                    // info!("forwarding request from {}", upstream_server);

                                    let response = JsonRpcForwardedResponse {
                                        jsonrpc: "2.0".to_string(),
                                        id: request.id,
                                        // TODO: since we only use the result here, should that be all we return from try_send_request?
                                        result: Some(partial_response),
                                        error: None,
                                    };

                                    // TODO: small race condidition here. parallel requests with the same query will both be saved to the cache
                                    // TODO: try_write is unlikely to get the lock. i think instead we should spawn another task for caching
                                    if let Ok(mut response_cache) = self.response_cache.try_write()
                                    {
                                        // TODO: cache the warp::reply to save us serializing every time
                                        response_cache.insert(cache_key, response.clone());
                                        if response_cache.len() >= RESPONSE_CACHE_CAP {
                                            // TODO: this isn't really an LRU. what is this called? should we make it an lru? these caches only live for one block
                                            response_cache.pop_front();
                                        }
                                    }

                                    response
                                }
                                Err(e) => {
                                    // TODO: move this to a helper function?
                                    let code;
                                    let message: String;
                                    let data;

                                    match e {
                                        ProviderError::JsonRpcClientError(e) => {
                                            // TODO: we should check what type the provider is rather than trying to downcast both types of errors
                                            if let Some(e) = e.downcast_ref::<HttpClientError>() {
                                                match &*e {
                                                    HttpClientError::JsonRpcError(e) => {
                                                        code = e.code;
                                                        message = e.message.clone();
                                                        data = e.data.clone();
                                                    }
                                                    e => {
                                                        // TODO: improve this
                                                        code = -32603;
                                                        message = format!("{}", e);
                                                        data = None;
                                                    }
                                                }
                                            } else if let Some(e) =
                                                e.downcast_ref::<WsClientError>()
                                            {
                                                match &*e {
                                                    WsClientError::JsonRpcError(e) => {
                                                        code = e.code;
                                                        message = e.message.clone();
                                                        data = e.data.clone();
                                                    }
                                                    e => {
                                                        // TODO: improve this
                                                        code = -32603;
                                                        message = format!("{}", e);
                                                        data = None;
                                                    }
                                                }
                                            } else {
                                                unimplemented!();
                                            }
                                        }
                                        _ => {
                                            code = -32603;
                                            message = format!("{}", e);
                                            data = None;
                                        }
                                    }

                                    JsonRpcForwardedResponse {
                                        jsonrpc: "2.0".to_string(),
                                        id: request.id,
                                        result: None,
                                        error: Some(JsonRpcErrorData {
                                            code,
                                            message,
                                            data,
                                        }),
                                    }
                                }
                            };

                            if response.error.is_some() {
                                trace!("Sending error reply: {:?}", response);
                            } else {
                                trace!("Sending reply: {:?}", response);
                            }

                            return Ok(response);
                        }
                        Err(None) => {
                            // TODO: this is too verbose. if there are other servers in other tiers, we use those!
                            // warn!("No servers in sync!");
                        }
                        Err(Some(not_until)) => {
                            // save the smallest not_until. if nothing succeeds, return an Err with not_until in it
                            // TODO: helper function for this
                            if earliest_not_until.is_none() {
                                earliest_not_until.replace(not_until);
                            } else {
                                let earliest_possible =
                                    earliest_not_until.as_ref().unwrap().earliest_possible();

                                let new_earliest_possible = not_until.earliest_possible();

                                if earliest_possible > new_earliest_possible {
                                    earliest_not_until = Some(not_until);
                                }
                            }
                        }
                    }
                }

                // we haven't returned an Ok
                // if we did return a rate limit error, sleep and try again
                if let Some(earliest_not_until) = earliest_not_until {
                    let deadline = earliest_not_until.wait_time_from(self.clock.now());

                    // TODO: max wait

                    sleep(deadline).await;
                } else {
                    // TODO: how long should we wait?
                    // TODO: max wait time?
                    warn!("No servers in sync!");
                    // TODO: return a 502?
                    return Err(anyhow::anyhow!("no servers in sync"));
                };
            }
        }
    }
}
