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
        with_lock: bool,
    ) -> Result<Arc<NotificationSync>, ClientError> {
        RUNTIME.block_on(async move {
            let inner = Arc::new(
                MatrixNotificationSync::new(id, self.inner.clone(), mode, with_lock).await?,
            );

            let handle = NotificationSync::start(inner.clone(), listener);

            Ok(Arc::new(NotificationSync { _handle: handle, sync: inner }))
        })
    }
}

#[uniffi::export]
impl Client {
    /// Must be called to get the encryption loop running.
    ///
    /// `id` must be a unique identifier, less than 16 chars long, for the
    /// current process. It must not change over time, as it's used as a key
    /// for caching.
    ///
    /// If the process involves another process that handles notifications (like
    /// on iOS), then `with_lock` must be set to true. Otherwise, it can be
    /// false (like on Android).
    pub fn main_encryption_loop(
        &self,
        id: String,
        listener: Box<dyn NotificationSyncListener>,
        with_lock: bool,
    ) -> Result<Arc<NotificationSync>, ClientError> {
        self.notification_sync(id, listener, NotificationSyncMode::NeverStop, with_lock)
    }

    /// Encryption loop for a notification process.
    ///
    /// Requires that the main process also gets its own encryption sync, with
    /// `with_lock` set to true.
    ///
    /// A fixed number of iterations can be given, to limit the time spent in
    /// that loop.
    pub fn notification_encryption_loop(
        &self,
        id: String,
        listener: Box<dyn NotificationSyncListener>,
        num_iters: u8,
    ) -> Result<Arc<NotificationSync>, ClientError> {
        let with_lock = true;
        self.notification_sync(
            id,
            listener,
            NotificationSyncMode::RunFixedIterations(num_iters),
            with_lock,
        )
    }
}
