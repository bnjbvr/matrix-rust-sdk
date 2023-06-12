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
//! handling the synchronization of notifications, be they received within the app or
//! within a dedicated notification process (e.g. the NSE process on iOS devices).
//!
//! Under the hood, this uses a sliding sync instance configured with no lists, but that enables
//! the e2ee and to-device extensions, so that it can both handle encryption et manage encryption
//! keys; that's sufficient to decrypt messages received in the notification processes.
//!
//! As this may be used across different processes, this also makes sure that there's only one
//! process writing to the databases holding encryption information.
//! TODO as of 2023-06-06, this hasn't been done yet.

use async_stream::stream;
use futures_core::stream::Stream;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{sliding_sync::ExtensionsSetter, Client, SlidingSync};
use tracing::warn;

/// High-level helper for synchronizing notifications using sliding sync.
///
/// See the module's documentation for more details.
pub struct NotificationApi {
    sliding_sync: SlidingSync,
}

impl NotificationApi {
    /// Creates a new instance of a `NotificationApi`.
    ///
    /// This will create and manage an instance of [`matrix_sdk::SlidingSync`]. The `id` is used as
    /// the identifier of that instance, as such make sure to not reuse a name used
    /// by another Sliding Sync instance, at the risk of causing problems.
    pub async fn new(id: impl Into<String>, client: Client) -> Result<Self, Error> {
        let sliding_sync = client
            .sliding_sync(id)
            .map_err(Error::SlidingSyncError)?
            .enable_caching()?
            .with_extensions(ExtensionsSetter {
                to_device: true,
                e2ee: true,
                account_data: false,
                receipts: false,
                typing: false,
            })
            .build()
            .await?;

        Ok(Self { sliding_sync })
    }

    /// Start synchronization of notifications.
    ///
    /// This should be regularly polled, so as to ensure that the notifications are sync'd.
    pub fn sync(&self) -> impl Stream<Item = Result<(), Error>> + '_ {
        stream!({
            let sync = self.sliding_sync.sync();

            pin_mut!(sync);

            loop {
                match sync.next().await {
                    Some(Ok(update_summary)) => {
                        // This API is only concerned with the e2ee and to-device extensions.
                        // Warn if anything weird has been received from the proxy.
                        if !update_summary.lists.is_empty() {
                            warn!(?update_summary.lists, "unexpected non-empty list of lists in notification API");
                        }
                        if !update_summary.rooms.is_empty() {
                            warn!(?update_summary.rooms, "unexpected non-empty list of rooms in notification API");
                        }

                        // Cool cool, let's do it again.
                        yield Ok(());

                        continue;
                    }

                    Some(Err(err)) => {
                        yield Err(err.into());

                        break;
                    }

                    None => {
                        break;
                    }
                }
            }
        })
    }
}

/// Errors for the [`NotificationApi`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Something wrong happened in sliding sync")]
    SlidingSyncError(#[from] matrix_sdk::Error),
}
