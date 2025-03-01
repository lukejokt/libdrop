use std::{
    collections::HashMap,
    fs,
    net::IpAddr,
    ops::ControlFlow,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;
use drop_config::DropConfig;
use futures::{SinkExt, StreamExt};
use sha1::Digest;
use slog::{debug, error, warn};
use tokio::{
    sync::mpsc::{self, Sender, UnboundedSender},
    task::JoinHandle,
};
use warp::ws::{Message, WebSocket};

use super::{handler, ServerReq};
use crate::{
    file::FileSubPath,
    protocol::v2,
    service::State,
    utils::Hidden,
    ws::{self, events::FileEventTx},
    FileId,
};

pub struct HandlerInit<'a, const PING: bool = true> {
    peer: IpAddr,
    state: &'a Arc<State>,
    logger: &'a slog::Logger,
}

pub struct HandlerLoop<'a, const PING: bool> {
    state: &'a Arc<State>,
    logger: &'a slog::Logger,
    msg_tx: Sender<Message>,
    xfer: crate::Transfer,
    last_recv: Instant,
    jobs: HashMap<FileSubPath, FileTask>,
}

struct Downloader {
    file_id: FileSubPath,
    msg_tx: Sender<Message>,
    tmp_loc: Option<Hidden<PathBuf>>,
}

struct FileTask {
    job: JoinHandle<()>,
    chunks_tx: UnboundedSender<Vec<u8>>,
    events: Arc<FileEventTx>,
}

impl<'a, const PING: bool> HandlerInit<'a, PING> {
    pub(crate) fn new(peer: IpAddr, state: &'a Arc<State>, logger: &'a slog::Logger) -> Self {
        Self {
            peer,
            state,
            logger,
        }
    }
}

#[async_trait::async_trait]
impl<'a, const PING: bool> handler::HandlerInit for HandlerInit<'a, PING> {
    type Request = (v2::TransferRequest, IpAddr, Arc<DropConfig>);
    type Loop = HandlerLoop<'a, PING>;
    type Pinger = ws::utils::Pinger<PING>;

    async fn recv_req(&mut self, ws: &mut WebSocket) -> anyhow::Result<Self::Request> {
        let msg = ws
            .next()
            .await
            .context("Did not received transfer request")?
            .context("Failed to receive transfer request")?;

        let msg = msg.to_str().ok().context("Expected JOSN message")?;

        let req = serde_json::from_str(msg).context("Failed to deserialize transfer request")?;

        Ok((req, self.peer, self.state.config.clone()))
    }

    async fn on_error(&mut self, ws: &mut WebSocket, err: anyhow::Error) -> anyhow::Result<()> {
        let msg = v2::ServerMsg::Error(v2::Error {
            file: None,
            msg: err.to_string(),
        });

        ws.send(Message::from(&msg))
            .await
            .context("Failed to send error message")?;
        Ok(())
    }

    async fn upgrade(
        self,
        _: &mut WebSocket,
        msg_tx: Sender<Message>,
        xfer: crate::Transfer,
    ) -> Option<Self::Loop> {
        let Self {
            peer: _,
            state,
            logger,
        } = self;

        Some(HandlerLoop {
            state,
            msg_tx,
            xfer,
            last_recv: Instant::now(),
            jobs: HashMap::new(),
            logger,
        })
    }

    fn pinger(&mut self) -> Self::Pinger {
        ws::utils::Pinger::<PING>::new(self.state)
    }
}

impl<const PING: bool> HandlerLoop<'_, PING> {
    fn issue_download(
        &mut self,
        _: &mut WebSocket,
        task: super::FileXferTask,
    ) -> anyhow::Result<()> {
        let is_running = self
            .jobs
            .get(task.file.subpath())
            .map_or(false, |state| !state.job.is_finished());

        if is_running {
            return Ok(());
        }

        let subpath = task.file.subpath().clone();
        let state = FileTask::start(
            self.msg_tx.clone(),
            self.state.clone(),
            task,
            self.logger.clone(),
        );

        self.jobs.insert(subpath, state);

        Ok(())
    }

    async fn issue_cancel(
        &mut self,
        socket: &mut WebSocket,
        file_id: FileId,
    ) -> anyhow::Result<()> {
        debug!(self.logger, "ServerHandler::issue_cancel");

        let file_subpath = if let Some(file) = self.xfer.files().get(&file_id) {
            file.subpath().clone()
        } else {
            warn!(self.logger, "Missing file with ID: {file_id:?}");
            return Ok(());
        };

        let msg = v2::ServerMsg::Cancel(v2::Download {
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
        debug!(self.logger, "ServerHandler::issue_cancel");

        let file_subpath = if let Some(file) = self.xfer.files().get(&file_id) {
            file.subpath().clone()
        } else {
            warn!(self.logger, "Missing file with ID: {file_id:?}");
            return Ok(());
        };

        let msg = v2::ServerMsg::Cancel(v2::Download {
            file: file_subpath.clone(),
        });
        socket.send(Message::from(&msg)).await?;

        if let Some(FileTask {
            job: task,
            events,
            chunks_tx: _,
        }) = self.jobs.remove(&file_subpath)
        {
            if !task.is_finished() {
                task.abort();

                events
                    .stop(crate::Event::FileDownloadCancelled(
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
                .send(crate::Event::FileDownloadRejected {
                    transfer_id: self.xfer.id(),
                    file_id,
                    by_peer: false,
                })
                .await
                .expect("Event channel should be open");
        }

        Ok(())
    }

    async fn on_chunk(
        &mut self,
        socket: &mut WebSocket,
        file: FileSubPath,
        chunk: Vec<u8>,
    ) -> anyhow::Result<()> {
        if let Some(task) = self.jobs.get(&file) {
            if let Err(err) = task.chunks_tx.send(chunk) {
                let msg = v2::Error {
                    msg: format!("Failed to consue chunk for file: {file:?}, msg: {err}",),
                    file: Some(file),
                };

                socket
                    .send(Message::from(&v2::ServerMsg::Error(msg)))
                    .await?;
            }
        }

        Ok(())
    }

    async fn on_cancel(&mut self, file: FileSubPath, by_peer: bool) {
        if let Some(FileTask {
            job: task,
            events,
            chunks_tx: _,
        }) = self.jobs.remove(&file)
        {
            if !task.is_finished() {
                task.abort();
                let file = self
                    .xfer
                    .file_by_subpath(&file)
                    .expect("File should exists since we have a transfer task running");

                self.state.moose.service_quality_transfer_file(
                    Err(u32::from(&crate::Error::Canceled) as i32),
                    drop_analytics::Phase::End,
                    self.xfer.id().to_string(),
                    0,
                    file.info(),
                );

                events
                    .stop(crate::Event::FileDownloadCancelled(
                        self.xfer.clone(),
                        file.id().clone(),
                        by_peer,
                    ))
                    .await;
            }
        }
    }

    async fn on_error(&mut self, file: Option<FileSubPath>, msg: String) {
        error!(
            self.logger,
            "Client reported and error: file: {:?}, message: {}", file, msg
        );

        if let Some(file) = file {
            if let Some(FileTask {
                job: task,
                events,
                chunks_tx: _,
            }) = self.jobs.remove(&file)
            {
                if !task.is_finished() {
                    task.abort();

                    let file = self
                        .xfer
                        .file_by_subpath(&file)
                        .expect("File should exists since we have a transfer task running");

                    events
                        .stop(crate::Event::FileDownloadFailed(
                            self.xfer.clone(),
                            file.id().clone(),
                            crate::Error::BadTransferState(format!(
                                "Sender reported an error: {msg}"
                            )),
                        ))
                        .await;
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl<const PING: bool> handler::HandlerLoop for HandlerLoop<'_, PING> {
    async fn on_req(&mut self, ws: &mut WebSocket, req: ServerReq) -> anyhow::Result<()> {
        match req {
            ServerReq::Download { task } => self.issue_download(ws, *task)?,
            ServerReq::Cancel { file } => self.issue_cancel(ws, file).await?,
            ServerReq::Reject { file } => self.issue_reject(ws, file).await?,
        }

        Ok(())
    }

    async fn on_close(&mut self, by_peer: bool) {
        debug!(self.logger, "ServerHandler::on_close(by_peer: {})", by_peer);

        self.xfer
            .files()
            .values()
            .filter(|file| {
                self.jobs
                    .get(&file.subpath)
                    .map_or(false, |state| !state.job.is_finished())
            })
            .for_each(|file| {
                self.state.moose.service_quality_transfer_file(
                    Err(u32::from(&crate::Error::Canceled) as i32),
                    drop_analytics::Phase::End,
                    self.xfer.id().to_string(),
                    0,
                    file.info(),
                );
            });

        self.on_stop().await;

        self.state
            .event_tx
            .send(crate::Event::TransferCanceled(
                self.xfer.clone(),
                false,
                by_peer,
            ))
            .await
            .expect("Could not send a file cancelled event, channel closed");
    }

    async fn on_recv(
        &mut self,
        ws: &mut WebSocket,
        msg: Message,
    ) -> anyhow::Result<ControlFlow<()>> {
        self.last_recv = Instant::now();

        if let Ok(json) = msg.to_str() {
            let msg: v2::ClientMsg =
                serde_json::from_str(json).context("Failed to deserialize json")?;

            match msg {
                v2::ClientMsg::Error(v2::Error { file, msg }) => self.on_error(file, msg).await,
                v2::ClientMsg::Cancel(v2::Download { file }) => self.on_cancel(file, true).await,
            }
        } else if msg.is_binary() {
            let v2::Chunk { file, data } =
                v2::Chunk::decode(msg.into_bytes()).context("Failed to decode file chunk")?;

            self.on_chunk(ws, file, data).await?;
        } else if msg.is_close() {
            debug!(self.logger, "Got CLOSE frame");
            self.on_close(true).await;

            return Ok(ControlFlow::Break(()));
        } else if msg.is_ping() {
            debug!(self.logger, "PING");
        } else if msg.is_pong() {
            debug!(self.logger, "PONG");
        } else {
            warn!(self.logger, "Server received invalid WS message type");
        }

        anyhow::Ok(ControlFlow::Continue(()))
    }

    async fn on_stop(&mut self) {
        debug!(self.logger, "Waiting for background jobs to finish");

        let tasks = self.jobs.drain().map(|(_, task)| {
            task.job.abort();

            async move {
                task.events.stop_silent().await;
            }
        });

        futures::future::join_all(tasks).await;
    }

    async fn finalize_failure(self, err: anyhow::Error) {
        error!(self.logger, "Server failed to handle WS message: {:?}", err);

        let err = match err.downcast::<crate::Error>() {
            Ok(err) => err,
            Err(err) => err.downcast::<warp::Error>().map_or_else(
                |err| crate::Error::BadTransferState(err.to_string()),
                Into::into,
            ),
        };

        self.state
            .event_tx
            .send(crate::Event::TransferFailed(self.xfer.clone(), err, true))
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
        debug!(self.logger, "Stopping server handler");
        self.jobs.values().for_each(|task| task.job.abort());
    }
}

impl Downloader {
    async fn send(&mut self, msg: impl Into<Message>) -> crate::Result<()> {
        self.msg_tx
            .send(msg.into())
            .await
            .map_err(|_| crate::Error::Canceled)
    }
}

impl Drop for Downloader {
    fn drop(&mut self) {
        if let Some(path) = self.tmp_loc.as_ref() {
            let _ = fs::remove_file(&path.0);
        }
    }
}

#[async_trait::async_trait]
impl handler::Downloader for Downloader {
    async fn init(&mut self, task: &super::FileXferTask) -> crate::Result<handler::DownloadInit> {
        let mut suffix = sha1::Sha1::new();

        suffix.update(task.xfer.id().as_bytes());
        if let Ok(time) = SystemTime::now().elapsed() {
            suffix.update(time.as_nanos().to_ne_bytes());
        }
        let suffix: String = suffix
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();

        let tmp_location: Hidden<PathBuf> = Hidden(
            format!(
                "{}.dropdl-{}",
                task.absolute_path.display(),
                suffix.get(..8).unwrap_or(&suffix),
            )
            .into(),
        );

        super::validate_tmp_location_path(&tmp_location)?;

        let msg = v2::ServerMsg::Start(v2::Download {
            file: self.file_id.clone(),
        });
        self.send(Message::from(&msg)).await?;

        self.tmp_loc = Some(tmp_location.clone());
        Ok(handler::DownloadInit::Stream {
            offset: 0,
            tmp_location,
        })
    }

    async fn open(&mut self, path: &Hidden<PathBuf>) -> crate::Result<fs::File> {
        let file = fs::File::create(&path.0)?;
        Ok(file)
    }

    async fn progress(&mut self, bytes: u64) -> crate::Result<()> {
        self.send(&v2::ServerMsg::Progress(v2::Progress {
            file: self.file_id.clone(),
            bytes_transfered: bytes,
        }))
        .await
    }

    async fn done(&mut self, bytes: u64) -> crate::Result<()> {
        self.send(&v2::ServerMsg::Done(v2::Progress {
            file: self.file_id.clone(),
            bytes_transfered: bytes,
        }))
        .await
    }

    async fn error(&mut self, msg: String) -> crate::Result<()> {
        self.send(&v2::ServerMsg::Error(v2::Error {
            file: Some(self.file_id.clone()),
            msg,
        }))
        .await
    }

    async fn validate(&mut self, _: &Hidden<PathBuf>) -> crate::Result<()> {
        Ok(())
    }
}

impl FileTask {
    fn start(
        msg_tx: Sender<Message>,
        state: Arc<State>,
        task: super::FileXferTask,
        logger: slog::Logger,
    ) -> Self {
        let events = Arc::new(FileEventTx::new(&state));
        let (chunks_tx, chunks_rx) = mpsc::unbounded_channel();

        let downloader = Downloader {
            file_id: task.file.subpath().clone(),
            msg_tx,
            tmp_loc: None,
        };
        let job = tokio::spawn(task.run(state, Arc::clone(&events), downloader, chunks_rx, logger));

        Self {
            job,
            chunks_tx,
            events,
        }
    }
}

impl handler::Request for (v2::TransferRequest, IpAddr, Arc<DropConfig>) {
    fn parse(self) -> anyhow::Result<crate::Transfer> {
        self.try_into().context("Failed to parse transfer request")
    }
}
