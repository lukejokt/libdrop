use std::{
    collections::{hash_map::Entry, HashMap},
    ops::ControlFlow,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use futures::SinkExt;
use slog::{debug, error, warn};
use tokio::{sync::mpsc::Sender, task::JoinHandle};
use tokio_tungstenite::tungstenite::{self, Message};

use super::{handler, ClientReq, WebSocket};
use crate::{file::FileSubPath, protocol::v2, service::State, ws, FileId};

pub struct HandlerInit<'a, const PING: bool = true> {
    state: &'a Arc<State>,
    logger: &'a slog::Logger,
}

pub struct HandlerLoop<'a, const PING: bool> {
    state: &'a Arc<State>,
    logger: &'a slog::Logger,
    upload_tx: Sender<Message>,
    tasks: HashMap<FileSubPath, FileTask>,
    last_recv: Instant,
    xfer: crate::Transfer,
}

struct Uploader {
    sink: Sender<Message>,
    file_id: FileSubPath,
}

struct FileTask {
    job: JoinHandle<()>,
    events: Arc<ws::events::FileEventTx>,
}

impl<'a, const PING: bool> HandlerInit<'a, PING> {
    pub(crate) fn new(state: &'a Arc<State>, logger: &'a slog::Logger) -> Self {
        Self { state, logger }
    }
}

#[async_trait::async_trait]
impl<'a, const PING: bool> handler::HandlerInit for HandlerInit<'a, PING> {
    type Pinger = ws::utils::Pinger<PING>;
    type Loop = HandlerLoop<'a, PING>;

    async fn start(&mut self, socket: &mut WebSocket, xfer: &crate::Transfer) -> crate::Result<()> {
        let req = v2::TransferRequest::try_from(xfer)?;
        socket.send(Message::from(&req)).await?;
        Ok(())
    }

    fn upgrade(self, upload_tx: Sender<Message>, xfer: crate::Transfer) -> Self::Loop {
        let Self { state, logger } = self;

        HandlerLoop {
            state,
            logger,
            upload_tx,
            xfer,
            tasks: HashMap::new(),
            last_recv: Instant::now(),
        }
    }

    fn pinger(&mut self) -> Self::Pinger {
        ws::utils::Pinger::<PING>::new(self.state)
    }
}

impl<const PING: bool> HandlerLoop<'_, PING> {
    async fn issue_cancel(
        &mut self,
        socket: &mut WebSocket,
        file_id: FileId,
    ) -> anyhow::Result<()> {
        let file_subpath = if let Some(file) = self.xfer.files().get(&file_id) {
            file.subpath().clone()
        } else {
            warn!(self.logger, "Missing file with ID: {file_id:?}");
            return Ok(());
        };

        let msg = v2::ClientMsg::Cancel(v2::Download {
            file: file_subpath.clone(),
        });
        socket.send(Message::from(&msg)).await?;

        self.on_cancel(file_subpath, false).await;

        Ok(())
    }

    async fn issue_reject(
        &mut self,
        socket: &mut WebSocket,
        file_id: FileId,
    ) -> anyhow::Result<()> {
        let file_subpath = if let Some(file) = self.xfer.files().get(&file_id) {
            file.subpath().clone()
        } else {
            warn!(self.logger, "Missing file with ID: {file_id:?}");
            return Ok(());
        };

        let msg = v2::ClientMsg::Cancel(v2::Download {
            file: file_subpath.clone(),
        });
        socket.send(Message::from(&msg)).await?;

        if let Some(task) = self.tasks.remove(&file_subpath) {
            if !task.job.is_finished() {
                task.job.abort();

                task.events
                    .stop(crate::Event::FileUploadCancelled(
                        self.xfer.clone(),
                        file_id.clone(),
                        false,
                    ))
                    .await;
            }
        }

        if let Some(file) = self.xfer.files().get(&file_id) {
            self.state.moose.service_quality_transfer_file(
                Err(drop_core::Status::FileRejected as i32),
                drop_analytics::Phase::End,
                self.xfer.id().to_string(),
                0,
                file.info(),
            );

            self.state
                .event_tx
                .send(crate::Event::FileUploadRejected {
                    transfer_id: self.xfer.id(),
                    file_id,
                    by_peer: false,
                })
                .await
                .expect("Event channel should be open");
        }

        Ok(())
    }

    async fn on_cancel(&mut self, file: FileSubPath, by_peer: bool) {
        if let Some(task) = self.tasks.remove(&file) {
            if !task.job.is_finished() {
                task.job.abort();

                let file = self
                    .xfer
                    .file_by_subpath(&file)
                    .expect("File should exist since we have a transfer task running");

                self.state.moose.service_quality_transfer_file(
                    Err(u32::from(&crate::Error::Canceled) as i32),
                    drop_analytics::Phase::End,
                    self.xfer.id().to_string(),
                    0,
                    file.info(),
                );

                task.events
                    .stop(crate::Event::FileUploadCancelled(
                        self.xfer.clone(),
                        file.id().clone(),
                        by_peer,
                    ))
                    .await;
            }
        }
    }

    async fn on_progress(&self, file: FileSubPath, transfered: u64) {
        if let Some(task) = self.tasks.get(&file) {
            let file = self
                .xfer
                .file_by_subpath(&file)
                .expect("File should exist since we have a transfer task running");

            task.events
                .emit(crate::Event::FileUploadProgress(
                    self.xfer.clone(),
                    file.id().clone(),
                    transfered,
                ))
                .await;
        }
    }

    async fn on_done(&mut self, file: FileSubPath) {
        if let Some(task) = self.tasks.remove(&file) {
            let file = self
                .xfer
                .file_by_subpath(&file)
                .expect("File should exist since we have a transfer task running");

            task.events
                .stop(crate::Event::FileUploadSuccess(
                    self.xfer.clone(),
                    file.id().clone(),
                ))
                .await;
        }
    }

    async fn on_download(&mut self, file_id: FileSubPath) {
        let start = async {
            if let Some(file) = self.xfer.file_by_subpath(&file_id) {
                self.state
                    .transfer_manager
                    .lock()
                    .await
                    .ensure_file_not_rejected(self.xfer.id(), file.id())?;
            }

            match self.tasks.entry(file_id.clone()) {
                Entry::Occupied(o) => {
                    let task = o.into_mut();

                    if task.job.is_finished() {
                        *task = FileTask::new(
                            self.state,
                            Uploader {
                                sink: self.upload_tx.clone(),
                                file_id: file_id.clone(),
                            },
                            self.xfer.clone(),
                            file_id,
                            self.logger,
                        )
                        .await?;
                    } else {
                        anyhow::bail!("Transfer already in progress");
                    }
                }
                Entry::Vacant(v) => {
                    let task = FileTask::new(
                        self.state,
                        Uploader {
                            sink: self.upload_tx.clone(),
                            file_id: file_id.clone(),
                        },
                        self.xfer.clone(),
                        file_id,
                        self.logger,
                    )
                    .await?;

                    v.insert(task);
                }
            };

            anyhow::Ok(())
        };

        if let Err(err) = start.await {
            error!(self.logger, "Failed to start upload: {:?}", err);
        }
    }

    async fn on_error(&mut self, file: Option<FileSubPath>, msg: String) {
        error!(
            self.logger,
            "Server reported and error: file: {file:?}, message: {msg}",
        );

        if let Some(file) = file {
            if let Some(task) = self.tasks.remove(&file) {
                if !task.job.is_finished() {
                    task.job.abort();
                }

                let file = self
                    .xfer
                    .file_by_subpath(&file)
                    .expect("File should exist since we have a transfer task running");

                task.events
                    .stop(crate::Event::FileUploadFailed(
                        self.xfer.clone(),
                        file.id().clone(),
                        crate::Error::BadTransferState(format!(
                            "Receiver reported an error: {msg}"
                        )),
                    ))
                    .await;
            }
        }
    }
}

#[async_trait::async_trait]
impl<const PING: bool> handler::HandlerLoop for HandlerLoop<'_, PING> {
    async fn on_req(&mut self, socket: &mut WebSocket, req: ClientReq) -> anyhow::Result<()> {
        match req {
            ClientReq::Cancel { file } => self.issue_cancel(socket, file).await,
            ClientReq::Reject { file } => self.issue_reject(socket, file).await,
        }
    }

    async fn on_close(&mut self, by_peer: bool) {
        debug!(self.logger, "ClientHandler::on_close(by_peer: {})", by_peer);

        self.xfer
            .files()
            .values()
            .filter(|file| {
                self.tasks
                    .get(&file.subpath)
                    .map_or(false, |task| !task.job.is_finished())
            })
            .for_each(|file| {
                self.state.moose.service_quality_transfer_file(
                    Err(u32::from(&crate::Error::Canceled) as i32),
                    drop_analytics::Phase::End,
                    self.xfer.id().to_string(),
                    0,
                    file.info(),
                )
            });

        self.on_stop().await;

        self.state
            .event_tx
            .send(crate::Event::TransferCanceled(
                self.xfer.clone(),
                true,
                by_peer,
            ))
            .await
            .expect("Could not send a transfer cancelled event, channel closed");
    }

    async fn on_recv(
        &mut self,
        _: &mut WebSocket,
        msg: Message,
    ) -> anyhow::Result<ControlFlow<()>> {
        self.last_recv = Instant::now();

        match msg {
            Message::Text(json) => {
                let msg: v2::ServerMsg =
                    serde_json::from_str(&json).context("Failed to deserialize server message")?;

                match msg {
                    v2::ServerMsg::Progress(v2::Progress {
                        file,
                        bytes_transfered,
                    }) => self.on_progress(file, bytes_transfered).await,
                    v2::ServerMsg::Done(v2::Progress {
                        file,
                        bytes_transfered: _,
                    }) => self.on_done(file).await,
                    v2::ServerMsg::Error(v2::Error { file, msg }) => self.on_error(file, msg).await,
                    v2::ServerMsg::Start(v2::Download { file }) => self.on_download(file).await,
                    v2::ServerMsg::Cancel(v2::Download { file }) => {
                        self.on_cancel(file, true).await
                    }
                }
            }
            Message::Close(_) => {
                debug!(self.logger, "Got CLOSE frame");
                self.on_close(true).await;
                return Ok(ControlFlow::Break(()));
            }
            Message::Ping(_) => {
                debug!(self.logger, "PING");
            }
            Message::Pong(_) => {
                debug!(self.logger, "PONG");
            }
            _ => warn!(self.logger, "Client received invalid WS message type"),
        }

        Ok(ControlFlow::Continue(()))
    }

    async fn on_stop(&mut self) {
        debug!(self.logger, "Waiting for background jobs to finish");

        let tasks = self.tasks.drain().map(|(_, task)| {
            task.job.abort();

            async move {
                task.events.stop_silent().await;
            }
        });

        futures::future::join_all(tasks).await;
    }

    async fn finalize_failure(self, err: anyhow::Error) {
        error!(self.logger, "Client failed on WS loop: {:?}", err);

        let err = match err.downcast::<crate::Error>() {
            Ok(err) => err,
            Err(err) => err.downcast::<tungstenite::Error>().map_or_else(
                |err| crate::Error::BadTransferState(err.to_string()),
                Into::into,
            ),
        };

        self.state
            .event_tx
            .send(crate::Event::TransferFailed(self.xfer.clone(), err, false))
            .await
            .expect("Event channel should always be open");
    }

    fn recv_timeout(&mut self) -> Option<Duration> {
        if PING {
            Some(
                self.state
                    .config
                    .transfer_idle_lifetime
                    .saturating_sub(self.last_recv.elapsed()),
            )
        } else {
            None
        }
    }
}

impl<const PING: bool> Drop for HandlerLoop<'_, PING> {
    fn drop(&mut self) {
        debug!(self.logger, "Stopping client handler");
        self.tasks.values().for_each(|task| task.job.abort());
    }
}

#[async_trait::async_trait]
impl handler::Uploader for Uploader {
    async fn chunk(&mut self, chunk: &[u8]) -> Result<(), crate::Error> {
        let msg = v2::Chunk {
            file: self.file_id.clone(),
            data: chunk.to_vec(),
        };

        self.sink
            .send(Message::from(msg))
            .await
            .map_err(|_| crate::Error::Canceled)?;

        Ok(())
    }

    async fn error(&mut self, msg: String) {
        let msg = v2::ClientMsg::Error(v2::Error {
            file: Some(self.file_id.clone()),
            msg,
        });

        let _ = self.sink.send(Message::from(&msg)).await;
    }

    fn offset(&self) -> u64 {
        0
    }
}

impl FileTask {
    async fn new(
        state: &Arc<State>,
        uploader: Uploader,
        xfer: crate::Transfer,
        file: FileSubPath,
        logger: &slog::Logger,
    ) -> anyhow::Result<Self> {
        let events = Arc::new(ws::events::FileEventTx::new(state));

        let file_id = xfer
            .file_by_subpath(&file)
            .context("File not found")?
            .id()
            .clone();

        let job = super::start_upload(
            state.clone(),
            logger.clone(),
            Arc::clone(&events),
            uploader,
            xfer,
            file_id,
        )
        .await?;

        Ok(Self { job, events })
    }
}
