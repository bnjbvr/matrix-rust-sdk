use std::sync::Arc;

use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk_ui::notifications::{
    NotificationSync as MatrixNotificationSync, NotificationSyncMode,
};
use tracing::{error, info, warn};

use crate::{client::Client, error::ClientError, task_handle::TaskHandle, RUNTIME};

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

    sync: Arc<MatrixNotificationSync>,
}

impl NotificationSync {
    fn start(
        notification: Arc<MatrixNotificationSync>,
        listener: Box<dyn NotificationSyncListener>,
    ) -> TaskHandle {
        TaskHandle::new(RUNTIME.spawn(async move {
            let stream = notification.sync();
            pin_mut!(stream);

            loop {
                tokio::select! {
                    biased;

                    streamed = stream.next() => {
                        match streamed {
                            Some(Ok(())) => {
                                // Yay.
                            }

                            None => {
                                info!("Notification sliding sync ended");
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
        if let Err(err) = self.sync.stop() {
            error!("Error when stopping the notification sync: {err}");
        }
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
            let inner = Arc::new(MatrixNotificationSync::new(id, mode, self.inner.clone()).await?);

            let handle = NotificationSync::start(inner.clone(), listener);

            Ok(Arc::new(NotificationSync { _handle: handle, sync: inner }))
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
        self.notification_sync(id, listener, NotificationSyncMode::RunFixedIterations(num_iters))
    }
}
