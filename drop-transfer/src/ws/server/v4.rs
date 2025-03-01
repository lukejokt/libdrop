use std::{
    cmp::Ordering,
    collections::HashMap,
    fs, io,
    net::IpAddr,
    ops::ControlFlow,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use async_cell::sync::AsyncCell;
use drop_config::DropConfig;
use futures::{SinkExt, StreamExt};
use slog::{debug, error, info, warn};
use tokio::{
    sync::mpsc::{self, Sender, UnboundedSender},
    task::JoinHandle,
};
use warp::ws::{Message, WebSocket};

use super::{handler, ServerReq};
use crate::{file, protocol::v4, service::State, utils::Hidden, ws::events::FileEventTx, FileId};

pub struct HandlerInit<'a> {
    peer: IpAddr,
    state: Arc<State>,
    logger: &'a slog::Logger,
}

pub struct HandlerLoop<'a> {
    state: Arc<State>,
    logger: &'a slog::Logger,
    msg_tx: Sender<Message>,
    xfer: crate::Transfer,
    last_recv: Instant,
    jobs: HashMap<FileId, FileTask>,
    checksums: HashMap<FileId, Arc<AsyncCell<[u8; 32]>>>,
}

struct Downloader {
    logger: slog::Logger,
    file_id: FileId,
    msg_tx: Sender<Message>,
    csum_rx: mpsc::Receiver<v4::ReportChsum>,
    full_csum: Arc<AsyncCell<[u8; 32]>>,
    offset: u64,
}

struct FileTask {
    job: JoinHandle<()>,
    chunks_tx: UnboundedSender<Vec<u8>>,
    events: Arc<FileEventTx>,
    csum_tx: mpsc::Sender<v4::ReportChsum>,
}

impl<'a> HandlerInit<'a> {
    pub(crate) fn new(peer: IpAddr, state: Arc<State>, logger: &'a slog::Logger) -> Self {
        Self {
            peer,
            state,
            logger,
        }
    }
}

#[async_trait::async_trait]
impl<'a> handler::HandlerInit for HandlerInit<'a> {
    type Request = (v4::TransferRequest, IpAddr, Arc<DropConfig>);
    type Loop = HandlerLoop<'a>;
    type Pinger = tokio::time::Interval;

    async fn recv_req(&mut self, ws: &mut WebSocket) -> anyhow::Result<Self::Request> {
        let msg = ws
            .next()
            .await
            .context("Did not received transfer request")?
            .context("Failed to receive transfer request")?;

        let msg = msg.to_str().ok().context("Expected JOSN message")?;
        debug!(self.logger, "Request received:\n\t{msg}");

        let req = serde_json::from_str(msg).context("Failed to deserialize transfer request")?;

        Ok((req, self.peer, self.state.config.clone()))
    }

    async fn on_error(&mut self, ws: &mut WebSocket, err: anyhow::Error) -> anyhow::Result<()> {
        let msg = v4::ServerMsg::Error(v4::Error {
            file: None,
            msg: err.to_string(),
        });

        ws.send(Message::from(&msg))
            .await
            .context("Failed to send error message")?;
        Ok(())
    }

    async fn upgrade(
        mut self,
        ws: &mut WebSocket,
        msg_tx: Sender<Message>,
        xfer: crate::Transfer,
    ) -> Option<Self::Loop> {
        let task = async {
            let checksums = self
                .state
                .storage
                .fetch_checksums(xfer.id())
                .context("Failed to fetch fileche chsums from DB")?;

            let mut checksum_map = HashMap::new();
            let mut to_fetch = Vec::new();

            for (xfile, csum_bytes) in checksums.into_iter().filter_map(|csum| {
                let xfile = xfer.files().get(&csum.file_id)?;
                Some((xfile, csum.checksum))
            }) {
                let acell = checksum_map
                    .entry(xfile.id().clone())
                    .or_insert_with(AsyncCell::shared);

                match csum_bytes {
                    Some(csbytes) => acell.set(
                        csbytes
                            .try_into()
                            .ok()
                            .context("Invalid length checksum stored in the DB")?,
                    ),
                    None => to_fetch.push(xfile.id().clone()),
                }
            }

            Ok((to_fetch, checksum_map))
        };

        let (to_fetch, checksums) = match task.await {
            Ok(res) => res,
            Err(err) => {
                error!(self.logger, "Failed to prepare checksum info: {err}");

                let _ = self.on_error(ws, err).await;
                return None;
            }
        };

        let Self {
            peer: _,
            state,
            logger,
        } = self;

        // task responsible for requesting the checksum
        let req_file_checksums = {
            let msg_tx = msg_tx.clone();
            let logger = logger.clone();
            let xfer = xfer.clone();

            async move {
                for xfile in to_fetch.into_iter().filter_map(|id| xfer.files().get(&id)) {
                    let msg = v4::ReqChsum {
                        file: xfile.file_id.clone(),
                        limit: xfile.size(),
                    };
                    let msg = v4::ServerMsg::ReqChsum(msg);
                    if let Err(err) = msg_tx.send((&msg).into()).await {
                        warn!(logger, "Failed to request checksum: {err}");
                    }
                }
            }
        };
        tokio::spawn(req_file_checksums);

        Some(HandlerLoop {
            state,
            msg_tx,
            xfer,
            last_recv: Instant::now(),
            jobs: HashMap::new(),
            logger,
            checksums,
        })
    }

    fn pinger(&mut self) -> Self::Pinger {
        tokio::time::interval(self.state.config.ping_interval())
    }
}

impl HandlerLoop<'_> {
    fn issue_download(
        &mut self,
        _: &mut WebSocket,
        task: super::FileXferTask,
    ) -> anyhow::Result<()> {
        let is_running = self
            .jobs
            .get(task.file.id())
            .map_or(false, |state| !state.job.is_finished());

        if is_running {
            return Ok(());
        }

        let full_csum_cell = self
            .checksums
            .get(task.file.id())
            .context("Missing file checksum cell")?
            .clone();

        let file_id = task.file.id().clone();
        let state = FileTask::start(
            self.msg_tx.clone(),
            self.state.clone(),
            task,
            full_csum_cell,
            self.logger.clone(),
        );

        self.jobs.insert(file_id, state);

        Ok(())
    }

    async fn issue_cancel(
        &mut self,
        socket: &mut WebSocket,
        file_id: FileId,
    ) -> anyhow::Result<()> {
        debug!(self.logger, "ServerHandler::issue_cancel");

        let msg = v4::ServerMsg::Cancel(v4::Cancel {
            file: file_id.clone(),
        });
        socket.send(Message::from(&msg)).await?;

        self.on_cancel(file_id, false).await;

        Ok(())
    }

    async fn issue_reject(
        &mut self,
        socket: &mut WebSocket,
        file_id: FileId,
    ) -> anyhow::Result<()> {
        debug!(self.logger, "ServerHandler::issue_cancel");

        let msg = v4::ServerMsg::Cancel(v4::Cancel {
            file: file_id.clone(),
        });
        socket.send(Message::from(&msg)).await?;

        if let Some(FileTask {
            job: task,
            events,
            chunks_tx: _,
            csum_tx: _,
        }) = self.jobs.remove(&file_id)
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
        file_id: FileId,
        chunk: Vec<u8>,
    ) -> anyhow::Result<()> {
        if let Some(task) = self.jobs.get(&file_id) {
            if let Err(err) = task.chunks_tx.send(chunk) {
                let msg = v4::Error {
                    msg: format!("Failed to consume chunk for file: {file_id:?}, msg: {err}",),
                    file: Some(file_id),
                };

                socket
                    .send(Message::from(&v4::ServerMsg::Error(msg)))
                    .await?;
            }
        }

        Ok(())
    }

    async fn on_cancel(&mut self, file_id: FileId, by_peer: bool) {
        if let Some(FileTask {
            job: task,
            events,
            chunks_tx: _,
            csum_tx: _,
        }) = self.jobs.remove(&file_id)
        {
            if !task.is_finished() {
                task.abort();

                let file = self
                    .xfer
                    .files()
                    .get(&file_id)
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
                        file_id,
                        by_peer,
                    ))
                    .await;
            }
        }
    }

    async fn on_error(&mut self, file_id: Option<FileId>, msg: String) {
        error!(
            self.logger,
            "Client reported and error: file: {:?}, message: {}", file_id, msg
        );

        if let Some(file_id) = file_id {
            if let Some(FileTask {
                job: task,
                events,
                chunks_tx: _,
                csum_tx: _,
            }) = self.jobs.remove(&file_id)
            {
                if !task.is_finished() {
                    task.abort();

                    events
                        .stop(crate::Event::FileDownloadFailed(
                            self.xfer.clone(),
                            file_id,
                            crate::Error::BadTransferState(format!(
                                "Sender reported an error: {msg}"
                            )),
                        ))
                        .await;
                }
            }
        }
    }

    async fn on_checksum(&mut self, report: v4::ReportChsum) {
        let xfile = match self.xfer.files().get(&report.file) {
            Some(file) => file,
            None => return,
        };

        // Full checksum requsted at the begining of the transfer
        if report.limit == xfile.size() {
            self.checksums
                .get(&report.file)
                .expect("Missing file")
                .or_set(report.checksum);

            let storage = self.state.storage.clone();
            let transfer_id = self.xfer.id();
            let file_id = report.file.clone();
            let logger = self.logger.clone();

            tokio::spawn(async move {
                if let Err(err) =
                    storage.save_checksum(transfer_id, file_id.as_ref(), &report.checksum)
                {
                    error!(logger, "Failed to save checksum into DB: {err}");
                }
            });
        // Requests made by the download task
        } else if let Some(job) = self.jobs.get_mut(&report.file) {
            if job.csum_tx.send(report).await.is_err() {
                warn!(
                    self.logger,
                    "Failed to pass checksum report to receiver task"
                );
            }
        }
    }
}

#[async_trait::async_trait]
impl handler::HandlerLoop for HandlerLoop<'_> {
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
                    .get(file.id())
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
            debug!(self.logger, "Received:\n\t{json}");

            let msg: v4::ClientMsg =
                serde_json::from_str(json).context("Failed to deserialize json")?;

            match msg {
                v4::ClientMsg::Error(v4::Error { file, msg }) => self.on_error(file, msg).await,
                v4::ClientMsg::Cancel(v4::Cancel { file }) => self.on_cancel(file, true).await,
                v4::ClientMsg::ReportChsum(report) => self.on_checksum(report).await,
            }
        } else if msg.is_binary() {
            let v4::Chunk { file, data } =
                v4::Chunk::decode(msg.into_bytes()).context("Failed to decode file chunk")?;

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
        Some(
            self.state
                .config
                .transfer_idle_lifetime
                .saturating_sub(self.last_recv.elapsed()),
        )
    }
}

impl Drop for HandlerLoop<'_> {
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

    async fn request_csum(&mut self, limit: u64) -> crate::Result<v4::ReportChsum> {
        let msg = v4::ServerMsg::ReqChsum(v4::ReqChsum {
            file: self.file_id.clone(),
            limit,
        });
        self.send(Message::from(&msg)).await?;

        let report = self.csum_rx.recv().await.ok_or(crate::Error::Canceled)?;

        Ok(report)
    }
}

#[async_trait::async_trait]
impl handler::Downloader for Downloader {
    async fn init(&mut self, task: &super::FileXferTask) -> crate::Result<handler::DownloadInit> {
        let filename_len = task
            .absolute_path
            .file_name()
            .expect("Cannot extract file name")
            .len();

        if filename_len + super::MAX_FILE_SUFFIX_LEN > super::MAX_FILENAME_LENGTH {
            return Err(crate::Error::FilenameTooLong);
        }

        let tmp_filename = if cfg!(target_os = "android") {
            format!(
                "{}-{}.dropdl-part",
                task.xfer.id().as_simple(),
                task.file.id()
            )
        } else {
            format!("{}.dropdl-part", task.file.id())
        };

        let tmp_location: Hidden<PathBuf> =
            Hidden(task.absolute_path.0.with_file_name(tmp_filename));

        // Check if we can resume the temporary file
        match tokio::task::block_in_place(|| super::TmpFileState::load(&tmp_location.0)) {
            Ok(super::TmpFileState { meta, csum }) => {
                debug!(
                    self.logger,
                    "Found temporary file: {tmp_location:?}, of size: {}",
                    meta.len()
                );

                self.offset = match meta.len().cmp(&task.file.size()) {
                    Ordering::Less => {
                        let report = self.request_csum(meta.len()).await?;

                        if report.limit == meta.len() && report.checksum == csum {
                            // All matches, we can continue with temp file
                            meta.len()
                        } else {
                            info!(
                                self.logger,
                                "Found missmatch in partially downloaded file, overwriting"
                            );

                            0
                        }
                    }
                    Ordering::Equal => {
                        if self.full_csum.get().await == csum {
                            // All matches the temp file is actually the full file
                            meta.len()
                        } else {
                            info!(
                                self.logger,
                                "The partially downloaded file has the same size as the target \
                                 file but the checksum does not match, overwriting"
                            );

                            0
                        }
                    }
                    Ordering::Greater => {
                        info!(
                            self.logger,
                            "The partially downloaded file is bigger then the target file, \
                             overwriting"
                        );

                        0
                    }
                };
            }
            Err(err) => {
                debug!(self.logger, "Failed to load temporary file info: {err}");
            }
        };

        let msg = v4::ServerMsg::Start(v4::Start {
            file: self.file_id.clone(),
            offset: self.offset,
        });
        self.send(Message::from(&msg)).await?;

        Ok(handler::DownloadInit::Stream {
            offset: self.offset,
            tmp_location,
        })
    }

    async fn open(&mut self, path: &Hidden<PathBuf>) -> crate::Result<fs::File> {
        let file = if self.offset == 0 {
            fs::File::create(&path.0)?
        } else {
            fs::File::options().append(true).open(&path.0)?
        };

        Ok(file)
    }

    async fn progress(&mut self, bytes: u64) -> crate::Result<()> {
        self.send(&v4::ServerMsg::Progress(v4::Progress {
            file: self.file_id.clone(),
            bytes_transfered: bytes,
        }))
        .await
    }

    async fn done(&mut self, bytes: u64) -> crate::Result<()> {
        self.send(&v4::ServerMsg::Done(v4::Done {
            file: self.file_id.clone(),
            bytes_transfered: bytes,
        }))
        .await
    }

    async fn error(&mut self, msg: String) -> crate::Result<()> {
        self.send(&v4::ServerMsg::Error(v4::Error {
            file: Some(self.file_id.clone()),
            msg,
        }))
        .await
    }

    async fn validate(&mut self, path: &Hidden<PathBuf>) -> crate::Result<()> {
        let csum = tokio::task::block_in_place(|| {
            let file = std::fs::File::open(&path.0)?;
            let csum = file::checksum(&mut io::BufReader::new(file))?;
            crate::Result::Ok(csum)
        })?;

        if self.full_csum.get().await != csum {
            return Err(crate::Error::ChecksumMismatch);
        }

        Ok(())
    }
}

impl FileTask {
    fn start(
        msg_tx: Sender<Message>,
        state: Arc<State>,
        task: super::FileXferTask,
        full_csum: Arc<AsyncCell<[u8; 32]>>,
        logger: slog::Logger,
    ) -> Self {
        let events = Arc::new(FileEventTx::new(&state));
        let (chunks_tx, chunks_rx) = mpsc::unbounded_channel();
        let (csum_tx, csum_rx) = mpsc::channel(4);

        let downloader = Downloader {
            file_id: task.file.id().clone(),
            msg_tx,
            logger: logger.clone(),
            csum_rx,
            full_csum,
            offset: 0,
        };
        let job = tokio::spawn(task.run(state, Arc::clone(&events), downloader, chunks_rx, logger));

        Self {
            job,
            chunks_tx,
            events,
            csum_tx,
        }
    }
}
