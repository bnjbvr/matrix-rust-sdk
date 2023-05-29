use crate::{sliding_sync::RequestConfig, Client};
use ruma::{api::client::sync::sync_events::v4, assign};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{spawn, sync::Mutex as AsyncMutex};
use tracing::{debug, error, instrument, trace, Instrument as _, Span};
use url::Url;

#[derive(Clone)]
pub struct ToDeviceLoop {
    client: Client,
    since_token: Arc<RwLock<Option<String>>>,
    /// Customize the homeserver for sliding sync only
    homeserver: Option<Url>,
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

impl ToDeviceLoop {
    #[instrument(skip_all, fields(since))]
    async fn sync_once(&self) -> Result<(), crate::Error> {
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
                // TODO add the conn_id here
                assign!(v4::Request::new(), {
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
    async fn handle_response(
        &self,
        sliding_sync_response: v4::Response,
    ) -> Result<(), crate::Error> {
        // Transform a Sliding Sync Response to a `SyncResponse`.
        //
        // We may not need the `sync_response` in the future (once `SyncResponse` will
        // move to Sliding Sync, i.e. to `v4::Response`), but processing the
        // `sliding_sync_response` is vital, so it must be done somewhere; for now it
        // happens here.
        let sync_response = self.client.process_sliding_sync(&sliding_sync_response).await?;

        debug!(?sync_response, "To-device sliding sync response has been handled by the client");

        if let Some(response) = sliding_sync_response.extensions.to_device {
            let mut since_token = self.since_token.write().unwrap();
            *since_token = Some(response.next_batch);
        }

        Ok(())
    }

    async fn cache_to_storage(&self) -> Result<(), crate::Error> {
        let Some(storage_key) = self.storage_key.as_ref() else { return Ok(()) };

        trace!(storage_key, "Saving a ToDeviceSyncLoop");
        let storage = self.client.store();

        // Write this `SlidingSync` instance, as a `FrozenSlidingSync` instance, inside
        // the store.
        storage
            .set_custom_value(
                format!("{storage_key}-to-device-loop").as_bytes(),
                serde_json::to_vec(&FrozenToDeviceLoop::from(self))?,
            )
            .await?;

        Ok(())
    }
}
