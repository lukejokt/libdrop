use std::collections::HashMap;

use drop_storage::{
    error::Error,
    types::{Event, TransferFiles},
    Storage, TransferType,
};
use uuid::Uuid;

pub struct StorageDispatch<'a> {
    storage: &'a drop_storage::Storage,
    file_progress: HashMap<(Uuid, String), i64>,
}

impl<'a> StorageDispatch<'a> {
    pub fn new(storage: &'a Storage) -> Self {
        Self {
            storage,
            file_progress: HashMap::new(),
        }
    }

    pub fn handle_event(&mut self, event: &crate::Event) -> Result<(), Error> {
        let event = Into::<Event>::into(event);
        match event {
            Event::Pending { transfer_info } => match &transfer_info.files {
                TransferFiles::Incoming(files) => {
                    for file in files {
                        self.storage
                            .insert_incoming_path_pending_state(transfer_info.id, &file.file_id)?
                    }
                }
                TransferFiles::Outgoing(files) => {
                    for file in files {
                        self.storage
                            .insert_outgoing_path_pending_state(transfer_info.id, &file.file_id)?
                    }
                }
            },

            Event::FileUploadStarted {
                transfer_id,
                file_id,
            } => self
                .storage
                .insert_outgoing_path_started_state(transfer_id, &file_id)?,

            Event::FileDownloadStarted {
                transfer_id,
                file_id,
                base_dir,
            } => {
                self.storage
                    .insert_incoming_path_started_state(transfer_id, &file_id, &base_dir)?
            }

            Event::FileCanceled {
                transfer_type,
                transfer_id,
                file_id,
                by_peer,
            } => match transfer_type {
                TransferType::Incoming => {
                    let progress = self.get_file_progress(transfer_id, &file_id);
                    self.storage.insert_incoming_path_cancel_state(
                        transfer_id,
                        &file_id,
                        by_peer,
                        progress,
                    )?
                }
                TransferType::Outgoing => {
                    let progress = self.get_file_progress(transfer_id, &file_id);
                    self.storage.insert_outgoing_path_cancel_state(
                        transfer_id,
                        &file_id,
                        by_peer,
                        progress,
                    )?
                }
            },

            Event::FileDownloadComplete {
                transfer_id,
                file_id,
                final_path,
            } => self.storage.insert_incoming_path_completed_state(
                transfer_id,
                &file_id,
                &final_path,
            )?,

            Event::FileUploadComplete {
                transfer_id,
                file_id,
            } => self
                .storage
                .insert_outgoing_path_completed_state(transfer_id, &file_id)?,

            Event::TransferCanceled {
                transfer_type: _,
                transfer_info,
                by_peer,
            } => self
                .storage
                .insert_transfer_cancel_state(transfer_info.id, by_peer)?,

            Event::TransferFailed {
                transfer_type: _,
                transfer_info,
                error_code,
            } => self
                .storage
                .insert_transfer_failed_state(transfer_info.id, error_code)?,

            Event::FileFailed {
                transfer_type,
                transfer_id,
                file_id,
                error_code,
            } => {
                let progress = self.get_file_progress(transfer_id, &file_id);
                match transfer_type {
                    TransferType::Incoming => self.storage.insert_incoming_path_failed_state(
                        transfer_id,
                        &file_id,
                        error_code,
                        progress,
                    )?,
                    TransferType::Outgoing => self.storage.insert_outgoing_path_failed_state(
                        transfer_id,
                        &file_id,
                        error_code,
                        progress,
                    )?,
                }
            }

            Event::FileProgress {
                transfer_id,
                file_id,
                progress,
            } => {
                *self
                    .file_progress
                    .entry((transfer_id, file_id))
                    .or_default() = progress;
            }

            Event::FileReject {
                transfer_type,
                transfer_id,
                file_id,
                by_peer,
            } => match transfer_type {
                TransferType::Incoming => self.storage.insert_incoming_path_reject_state(
                    transfer_id,
                    &file_id,
                    by_peer,
                )?,
                TransferType::Outgoing => self.storage.insert_outgoing_path_reject_state(
                    transfer_id,
                    &file_id,
                    by_peer,
                )?,
            },
        }

        Ok(())
    }

    fn get_file_progress(&mut self, transfer_id: Uuid, file_id: &String) -> i64 {
        self.file_progress
            .remove(&(transfer_id, file_id.to_string()))
            .unwrap_or(0)
    }
}

impl From<&crate::Event> for Event {
    fn from(event: &crate::Event) -> Self {
        match event {
            crate::Event::RequestReceived(transfer) => Event::Pending {
                transfer_info: transfer.storage_info(),
            },
            crate::Event::RequestQueued(transfer) => Event::Pending {
                transfer_info: transfer.storage_info(),
            },
            crate::Event::FileDownloadStarted(transfer, file, base_dir) => {
                Event::FileDownloadStarted {
                    transfer_id: transfer.id(),
                    file_id: file.to_string(),
                    base_dir: base_dir.clone(),
                }
            }
            crate::Event::FileUploadStarted(transfer, file) => Event::FileUploadStarted {
                transfer_id: transfer.id(),
                file_id: file.to_string(),
            },
            crate::Event::FileDownloadCancelled(transfer, file, by_peer) => Event::FileCanceled {
                transfer_type: TransferType::Incoming,
                transfer_id: transfer.id(),
                file_id: file.to_string(),
                by_peer: *by_peer,
            },
            crate::Event::FileUploadCancelled(transfer, file, by_peer) => Event::FileCanceled {
                transfer_type: TransferType::Outgoing,
                transfer_id: transfer.id(),
                file_id: file.to_string(),
                by_peer: *by_peer,
            },
            crate::Event::FileDownloadSuccess(transfer, file) => Event::FileDownloadComplete {
                transfer_id: transfer.id(),
                file_id: file.id.to_string(),
                final_path: file.final_path.to_string_lossy().to_string(),
            },
            crate::Event::FileUploadSuccess(transfer, file) => Event::FileUploadComplete {
                transfer_id: transfer.id(),
                file_id: file.to_string(),
            },
            crate::Event::FileDownloadFailed(transfer, file, error) => Event::FileFailed {
                transfer_type: TransferType::Incoming,
                transfer_id: transfer.id(),
                file_id: file.to_string(),
                error_code: error.into(),
            },
            crate::Event::FileUploadFailed(transfer, file, error) => Event::FileFailed {
                transfer_type: TransferType::Outgoing,
                transfer_id: transfer.id(),
                file_id: file.to_string(),
                error_code: error.into(),
            },
            crate::Event::TransferCanceled(transfer, is_sender, by_peer) => {
                let transfer_type = match is_sender {
                    false => TransferType::Incoming,
                    true => TransferType::Outgoing,
                };

                Event::TransferCanceled {
                    transfer_type,
                    transfer_info: transfer.storage_info(),
                    by_peer: *by_peer,
                }
            }
            crate::Event::TransferFailed(transfer, error, by_peer) => {
                let transfer_type = match by_peer {
                    false => TransferType::Outgoing,
                    true => TransferType::Incoming,
                };

                Event::TransferFailed {
                    transfer_type,
                    transfer_info: transfer.storage_info(),
                    error_code: error.into(),
                }
            }
            crate::Event::FileDownloadProgress(transfer, file, progress) => Event::FileProgress {
                transfer_id: transfer.id(),
                file_id: file.to_string(),
                progress: *progress as i64,
            },
            crate::Event::FileUploadProgress(transfer, file, progress) => Event::FileProgress {
                transfer_id: transfer.id(),
                file_id: file.to_string(),
                progress: *progress as i64,
            },
            crate::Event::FileDownloadRejected {
                transfer_id,
                file_id,
                by_peer,
            } => Event::FileReject {
                transfer_type: TransferType::Incoming,
                transfer_id: *transfer_id,
                file_id: file_id.to_string(),
                by_peer: *by_peer,
            },
            crate::Event::FileUploadRejected {
                transfer_id,
                file_id,
                by_peer,
            } => Event::FileReject {
                transfer_type: TransferType::Outgoing,
                transfer_id: *transfer_id,
                file_id: file_id.to_string(),
                by_peer: *by_peer,
            },
        }
    }
}
