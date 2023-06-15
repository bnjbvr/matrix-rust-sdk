use std::sync::Arc;

use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk_ui::notifications::{
    NotificationSync as MatrixNotificationSync, NotificationSyncMode,
};
use tracing::{error, warn};

use crate::client::Client;
use crate::error::ClientError;
use crate::task_handle::TaskHandle;
use crate::RUNTIME;

#[uniffi::export(callback_interface)]
pub trait NotificationSyncListener: Sync + Send {
    /// Called whenever the notification sync loop terminates, and must be
    /// restarted.
    fn did_terminate(&self);
}

/// Full context for the notification sync loop.
#[derive(uniffi::Object)]
pub struct NotificationSync {
    /// Unused field, maintains the sliding sync loop alive.
    _handle: TaskHandle,

    internal_channel_sender: tokio::sync::mpsc::Sender<()>,
}

impl NotificationSync {
    fn start(
        notification: MatrixNotificationSync,
        listener: Box<dyn NotificationSyncListener>,
        mut internal_channel_receiver: tokio::sync::mpsc::Receiver<()>,
    ) -> TaskHandle {
        TaskHandle::new(RUNTIME.spawn(async move {
            let stream = notification.sync();
            pin_mut!(stream);

            loop {
                tokio::select! {
                    biased;

                    _ = internal_channel_receiver.recv() => {
                        // Abort the notification sync.
                        if let Err(err) = notification.stop().await {
                            error!("Error when shutting down the notification sync loop: {err}");
                        }

                        // Exit the loop.
                        break;
                    }

                    streamed = stream.next() => {
                        match streamed {
                            Some(Ok(())) => {
                                // Yay.
                            }

                            None => {
                                warn!("Notification sliding sync ended");
                                break;
                            }

                            Some(Err(err)) => {
                                // The internal sliding sync instance already handles retries for us, so if
                                // we get an error here, it means the maximum number of retries has been
                                // reached, and there's not much we can do anymore.
                                warn!("Error when handling notifications: {err}");
                                break;
                            }
                        }
                    }
                }
            }

            listener.did_terminate();
        }))
    }
}

#[uniffi::export]
impl NotificationSync {
    pub fn stop(&self) {
        RUNTIME.block_on(async {
            if let Err(err) = self.internal_channel_sender.send(()).await {
                error!("Error when requesting to stop notification sync: {err}");
            }
        });
    }
}

impl Client {
    fn notification_sync(
        &self,
        id: String,
        listener: Box<dyn NotificationSyncListener>,
        mode: NotificationSyncMode,
    ) -> Result<Arc<NotificationSync>, ClientError> {
        RUNTIME.block_on(async move {
            let inner = MatrixNotificationSync::new(id, mode, self.inner.clone()).await?;

            let (sender, receiver) = tokio::sync::mpsc::channel(8);

            let handle = NotificationSync::start(inner, listener, receiver);

            Ok(Arc::new(NotificationSync { _handle: handle, internal_channel_sender: sender }))
        })
    }
}

#[uniffi::export]
impl Client {
    pub fn main_notification_sync(
        &self,
        id: String,
        listener: Box<dyn NotificationSyncListener>,
    ) -> Result<Arc<NotificationSync>, ClientError> {
        self.notification_sync(id, listener, NotificationSyncMode::NeverStop)
    }

    pub fn nse_notification_loop(
        &self,
        id: String,
        listener: Box<dyn NotificationSyncListener>,
        num_iters: u8,
    ) -> Result<Arc<NotificationSync>, ClientError> {
        self.notification_sync(id, listener, NotificationSyncMode::RunFixedAttempts(num_iters))
    }
}
