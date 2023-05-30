use crate::{sliding_sync::RequestConfig, Client};
use ruma::{api::client::sync::sync_events::v4, assign};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{spawn, sync::Mutex as AsyncMutex};
use tracing::{debug, error, instrument, trace, warn, Instrument as _, Span};
use url::Url;

#[derive(Clone, Debug)]
pub struct ToDeviceLoop {
    /// The HTTP Matrix client.
    client: Client,

    /// Unique identifier of the role of this to-device loop.
    ///
    /// Must be less than 16 chars.
    connection_id: String,

    /// Customize the homeserver for sliding sync only.
    homeserver: Option<Url>,

    /// The to-devince "since" token, read from the previous `next_batch` response.
    ///
    /// Cached to / reloaded from the disk; may be none for the first request.
    since_token: Arc<RwLock<Option<String>>>,

    /// Latest position marker sent by the server.
    pos: Arc<RwLock<Option<String>>>,

    /// A lock to serialize processing of responses.
    response_handling_lock: Arc<AsyncMutex<()>>,

    storage_key: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct FrozenToDeviceLoop {
    since_token: Option<String>,
}

impl From<&ToDeviceLoop> for FrozenToDeviceLoop {
    fn from(to_device_loop: &ToDeviceLoop) -> Self {
        Self { since_token: to_device_loop.since_token.read().unwrap().clone() }
    }
}

struct FakeLock;

impl ToDeviceLoop {
    pub(super) fn new(
        connection_id: String,
        client: Client,
        storage_key: Option<String>,
        homeserver: Option<Url>,
    ) -> Self {
        // No need to reload the previous since_token from the cache; it's going to be done when reloading from the disk cache.
        Self {
            client,
            connection_id,
            homeserver,
            since_token: Default::default(),
            pos: Arc::new(RwLock::new(None)),
            response_handling_lock: Default::default(),
            storage_key,
        }
    }

    async fn acquire_lock(&self) -> Result<FakeLock, crate::Error> {
        // TODO wait for lock shared with other process.
        let lock = FakeLock;

        // Reload data from cache, reinitialize internal caches, etc.
        // TODO reinitialize internal caches
        self.restore_from_cache().await?;

        Ok(lock)
    }

    #[instrument(skip_all, fields(since))]
    pub async fn sync_once(&self) -> Result<(), crate::Error> {
        let _lock = self.acquire_lock().await?;

        let (request, request_config) = {
            let since = self.since_token.read().unwrap().clone();
            Span::current().record("since", &since);

            // Keep a small timeout, as the process running this loop might be short-lived.
            let timeout = Duration::from_secs(10);

            // Always request e2ee (so we get keys) as well as to-device events.
            let mut extensions = v4::ExtensionsConfig::default();
            extensions.e2ee.enabled = Some(true);
            extensions.to_device.enabled = Some(true);
            extensions.to_device.since = since;

            (
                // Build the request itself.
                assign!(v4::Request::new(), {
                    pos: self.pos.read().unwrap().clone(),
                    conn_id: Some(self.connection_id.clone()),
                    timeout: Some(timeout),
                    extensions,
                }),
                // Configure long-polling. We need 10 seconds for the long-poll itself, in
                // addition to 10 more extra seconds for the network delays.
                RequestConfig::default().timeout(timeout + Duration::from_secs(10)),
            )
        };

        debug!("Sending the to-device sliding sync request");

        // Prepare the request.
        let request = self.client.send_with_homeserver(
            request,
            Some(request_config),
            self.homeserver.as_ref().map(ToString::to_string),
        );

        // Send the request and get a response with end-to-end encryption support.
        //
        // Sending the `/sync` request out when end-to-end encryption is enabled means
        // that we need to also send out any outgoing e2ee related request out
        // coming from the `OlmMachine::outgoing_requests()` method.
        #[cfg(feature = "e2e-encryption")]
        let response = {
            debug!("To-device sliding sync loop is sending the request along with outgoing E2EE requests");
            let (e2ee_uploads, response) =
                futures_util::future::join(self.client.send_outgoing_requests(), request).await;
            if let Err(error) = e2ee_uploads {
                error!(?error, "Error while sending outgoing E2EE requests");
            }
            response
        }?;

        // Send the request and get a response _without_ end-to-end encryption support.
        #[cfg(not(feature = "e2e-encryption"))]
        let response = {
            debug!("To-device sliding sync is sending the request");
            request.await?
        };

        debug!("To-device sliding sync response received");

        // At this point, the request has been sent, and a response has been received.
        //
        // We must ensure the handling of the response cannot be stopped/
        // cancelled. It must be done entirely, otherwise we can have
        // corrupted/incomplete states for Sliding Sync and other parts of
        // the code.
        //
        // That's why we are running the handling of the response in a spawned
        // future that cannot be cancelled by anything.
        let this = self.clone();

        // Spawn a new future to ensure that the code inside this future cannot be
        // cancelled if this method is cancelled.
        let future = async move {
            debug!("To-device sliding sync response handling starts");

            // In case the task running this future is detached, we must
            // ensure responses are handled one at a time, hence we lock the
            // `response_handling_lock`.
            let _response_handling_lock = this.response_handling_lock.lock().await;

            // Handle the response.
            this.handle_response(response).await?;

            this.cache_to_storage().await?;

            debug!("To-device sliding sync response has been fully handled");
            Ok(())
        };

        spawn(future.instrument(Span::current())).await.unwrap()
    }

    /// Handle the HTTP response.
    async fn handle_response(&self, response: v4::Response) -> Result<(), crate::Error> {
        // Transform a Sliding Sync Response to a `SyncResponse`.
        //
        // We may not need the `sync_response` in the future (once `SyncResponse` will
        // move to Sliding Sync, i.e. to `v4::Response`), but processing the
        // `sliding_sync_response` is vital, so it must be done somewhere; for now it
        // happens here.
        let sync_response = self.client.process_sliding_sync(&response).await?;

        debug!(?sync_response, "To-device sliding sync response has been handled by the client");

        *self.pos.write().unwrap() = Some(response.pos);

        if let Some(to_device) = response.extensions.to_device {
            let mut since_token = self.since_token.write().unwrap();
            *since_token = Some(to_device.next_batch);
        }

        Ok(())
    }

    /// Returns the full storage key for this to-device sync loop.
    ///
    /// This must remain stable, as it's used as a key in the KV store.
    fn full_storage_key(&self, storage_key: &str) -> String {
        format!("{storage_key}-{}-to-device-loop", self.connection_id)
    }

    async fn cache_to_storage(&self) -> Result<(), crate::Error> {
        let Some(storage_key) = self.storage_key.as_ref() else { return Ok(()) };

        trace!(storage_key, "Saving a ToDeviceSyncLoop");
        let storage = self.client.store();

        // Write this `SlidingSync` instance, as a `FrozenSlidingSync` instance, inside
        // the store.
        storage
            .set_custom_value(
                self.full_storage_key(&storage_key).as_bytes(),
                serde_json::to_vec(&FrozenToDeviceLoop::from(self))?,
            )
            .await?;

        Ok(())
    }

    async fn restore_from_cache(&self) -> Result<(), crate::Error> {
        let Some(storage_key) = self.storage_key.as_ref() else { return Ok(()) };

        trace!(storage_key, "Restoring a ToDeviceSyncLoop");
        let storage = self.client.store();
        let key = self.full_storage_key(&storage_key);

        match storage
            .get_custom_value(key.as_bytes())
            .await?
            .map(|bytes| serde_json::from_slice::<FrozenToDeviceLoop>(&bytes))
        {
            Some(Ok(frozen)) => {
                // Only rewrite the token if it was set, as it should never transition from set -> non-set.
                if let Some(since_token) = frozen.since_token {
                    *self.since_token.write().unwrap() = Some(since_token);
                }
            }

            Some(Err(err)) => {
                warn!("to-device token in cache was corrupted, invalidating cache entry: {err:#}");
                let _ = storage.remove_custom_value(key.as_bytes());
            }

            None => {
                trace!("no to-device since token in the cache");
            }
        }

        Ok(())
    }

    /// Invalidates the current session, after the server has told us it doesn't know about some `pos` marker value.
    pub fn invalidate_session(&self) {
        // Reset the `pos` marker.
        *self.pos.write().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use wiremock::MockServer;

    use crate::test_utils::logged_in_client;

    use super::*;

    async fn new_to_device_loop() -> Result<(MockServer, ToDeviceLoop), crate::Error> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let to_device_loop = ToDeviceLoop::new(
            "connection_id".to_owned(),
            client,
            Some("storage_key".to_owned()),
            Some(Url::parse("https://slidingsync.example.org")?),
        );

        Ok((server, to_device_loop))
    }

    #[tokio::test]
    async fn test_to_device_token_properly_cached() -> Result<(), crate::Error> {
        let (_server, to_device_loop) = new_to_device_loop().await?;

        // When no to-device token is present, it's still not there after caching
        // either.
        let frozen = FrozenToDeviceLoop::from(&to_device_loop);
        assert!(frozen.since_token.is_none());

        // When a to-device token is present, it's properly reloaded.
        let since = String::from("my-to-device-since-token");
        *to_device_loop.since_token.write().unwrap() = Some(since.clone());

        let frozen = FrozenToDeviceLoop::from(&to_device_loop);
        assert_eq!(frozen.since_token, Some(since));

        Ok(())
    }
}
