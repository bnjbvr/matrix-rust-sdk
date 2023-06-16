// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for that specific language governing permissions and
// limitations under the License.

//! Notification API.
//!
//! The notification API is a high-level helper that is designed to take care of
//! handling the synchronization of notifications, be they received within the
//! app or within a dedicated notification process (e.g. the [NSE] process on
//! iOS devices).
//!
//! Under the hood, this uses a sliding sync instance configured with no lists,
//! but that enables the e2ee and to-device extensions, so that it can both
//! handle encryption et manage encryption keys; that's sufficient to decrypt
//! messages received in the notification processes.
//!
//! [NSE]: https://developer.apple.com/documentation/usernotifications/unnotificationserviceextension

use std::{
    ops::Not as _,
    sync::atomic::{AtomicI32, Ordering},
};

use async_stream::stream;
use futures_core::stream::Stream;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{Client, SlidingSync};
use matrix_sdk_base::crypto::store::locks::CryptoStoreLock;
use matrix_sdk_crypto::CryptoStoreError;
use ruma::{api::client::sync::sync_events::v4, assign};
use tracing::error;

pub enum NotificationSyncMode {
    RunFixedIterations(u8),
    NeverStop,
}

/// High-level helper for synchronizing notifications using sliding sync.
///
/// See the module's documentation for more details.
pub struct NotificationSync {
    client: Client,
    sliding_sync: SlidingSync,
    cross_process_lock: CryptoStoreLock,
    num_attempts: AtomicI32,
}

impl NotificationSync {
    /// Creates a new instance of a `NotificationSync`.
    ///
    /// This will create and manage an instance of [`matrix_sdk::SlidingSync`].
    /// The `id` is used as the identifier of that instance, as such make
    /// sure to not reuse a name used by another Sliding Sync instance, at
    /// the risk of causing problems.
    pub async fn new(
        id: impl Into<String>,
        mode: NotificationSyncMode,
        client: Client,
    ) -> Result<Self, Error> {
        let id = id.into();
        let sliding_sync = client
            .sliding_sync(id.clone())?
            .enable_caching()?
            .with_to_device_extension(
                assign!(v4::ToDeviceConfig::default(), { enabled: Some(true)}),
            )
            .with_e2ee_extension(assign!(v4::E2EEConfig::default(), { enabled: Some(true)}))
            .build()
            .await?;

        let cross_process_lock = client
            .encryption()
            .create_store_lock("write_lock".to_owned(), id)
            .await
            .map_err(|_| Error::AuthenticationRequired)?;

        let num_attempts = match mode {
            NotificationSyncMode::RunFixedIterations(val) => i32::from(val),
            NotificationSyncMode::NeverStop => -1,
        };

        Ok(Self { client, sliding_sync, cross_process_lock, num_attempts: num_attempts.into() })
    }

    /// Start synchronization of notifications.
    ///
    /// This should be regularly polled, so as to ensure that the notifications
    /// are sync'd.
    pub fn sync(&self) -> impl Stream<Item = Result<(), Error>> + '_ {
        stream!({
            let sync = self.sliding_sync.sync();

            pin_mut!(sync);

            loop {
                let num_attempts = self.num_attempts.load(Ordering::SeqCst);
                if num_attempts == 0 {
                    // If the previous attempt was the last one, stop now.
                    break;
                }

                if num_attempts > 0 {
                    // If there's a finite number of attempt, decrement.
                    self.num_attempts.store(num_attempts - 1, Ordering::SeqCst);
                }

                // Start by being pessimistic and assume some other process has the lock, so
                // writes to the custom stores are forbidden, in particular
                // preshare_room_key requests.
                let write_crypto_store_lock = self.client.preshare_room_key_lock();
                let write_crypto_store_guard = write_crypto_store_lock.lock().await;

                // Try to obtain the cross-process lock.
                if self.cross_process_lock.try_lock_once().await?.not() {
                    // HACK: this conditional is a way to recognize the NSE loop; need a better
                    // API for that.
                    if num_attempts != -1 {
                        // If we're in a notification process, and the lock is already taken, then
                        // the main app is running, and we let it do its work.
                        // TODO The embedder is expected to retry, in that case, but they can't be notified of it?
                        return;
                    }

                    // We didn't get the lock on the first time, so that means that another process
                    // is using it. Wait for it to release it.
                    self.cross_process_lock.spin_lock(Some(10000)).await?;

                    // As we didn't get the lock on the first attempt, force-reload all the crypto
                    // state at once, by recreating the OlmMachine.
                    if self.client.regenerate_olm().await.is_err() {
                        // First, give back the cross-process lock.
                        self.cross_process_lock.unlock().await?;

                        // Return the error by yielding and then stopping.
                        yield Err(Error::ReloadCryptoStoreError);
                        break;
                    };
                }

                // We obtained the cross-process lock. Now allow the preshare_room_key requests
                // to continue.
                drop(write_crypto_store_guard);

                match sync.next().await {
                    Some(Ok(update_summary)) => {
                        // This API is only concerned with the e2ee and to-device extensions.
                        // Warn if anything weird has been received from the proxy.
                        if !update_summary.lists.is_empty() {
                            error!(?update_summary.lists, "unexpected non-empty list of lists in notification API");
                        }
                        if !update_summary.rooms.is_empty() {
                            error!(?update_summary.rooms, "unexpected non-empty list of rooms in notification API");
                        }

                        self.cross_process_lock.unlock().await?;

                        // Cool cool, let's do it again.
                        yield Ok(());

                        continue;
                    }

                    Some(Err(err)) => {
                        self.cross_process_lock.unlock().await?;

                        yield Err(err.into());

                        break;
                    }

                    None => {
                        self.cross_process_lock.unlock().await?;
                        break;
                    }
                }
            }
        })
    }

    pub async fn stop(&self) -> Result<(), Error> {
        // Stopping the sync loop will cause the next `next()` call to return `None`, so this will
        // also release the cross-process lock automatically.

        self.sliding_sync.stop_sync()?;

        Ok(())
    }
}

/// Errors for the [`NotificationSync`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Something wrong happened in sliding sync: {0:#}")]
    SlidingSync(#[from] matrix_sdk::Error),

    #[error("The client was not authenticated")]
    AuthenticationRequired,

    #[error(transparent)]
    CryptoStore(#[from] CryptoStoreError),

    #[error("The crypto store state couldn't be reloaded")]
    ReloadCryptoStoreError,
}
