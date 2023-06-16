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

use std::sync::atomic::{AtomicI32, Ordering};

use async_stream::stream;
use futures_core::stream::Stream;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{Client, SlidingSync};

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

        // Gently try to set the cross-process lock on behalf of the user.
        match client.encryption().enable_cross_process_store_lock(id).await {
            Ok(()) | Err(matrix_sdk::Error::BadCryptoStoreState) => {
                // Ignore; we've already set the crypto store lock to something, and that's
                // sufficient as long as it uniquely identifies the process.
            }
            Err(err) => {
                // Any other error is fatal
                return Err(Error::ClientError(err));
            }
        };

        let num_attempts = match mode {
            NotificationSyncMode::RunFixedIterations(val) => i32::from(val),
            NotificationSyncMode::NeverStop => -1,
        };

        Ok(Self { client, sliding_sync, num_attempts: num_attempts.into() })
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

                let must_unlock = if num_attempts == -1 {
                    // Notification process.
                    self.client.encryption().try_lock_store_once().await?
                } else {
                    // Main process.
                    self.client.encryption().spin_lock_store(Some(60000)).await?;
                    true
                };

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

                        if must_unlock {
                            self.client.encryption().unlock_store().await?;
                        }

                        // Cool cool, let's do it again.
                        yield Ok(());

                        continue;
                    }

                    Some(Err(err)) => {
                        if must_unlock {
                            self.client.encryption().unlock_store().await?;
                        }

                        yield Err(err.into());

                        break;
                    }

                    None => {
                        if must_unlock {
                            self.client.encryption().unlock_store().await?;
                        }
                        break;
                    }
                }
            }
        })
    }

    pub fn stop(&self) -> Result<(), Error> {
        // Stopping the sync loop will cause the next `next()` call to return `None`, so
        // this will also release the cross-process lock automatically.
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

    #[error(transparent)]
    ClientError(matrix_sdk::Error),
}
