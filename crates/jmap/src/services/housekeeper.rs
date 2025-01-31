/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::{
    collections::BinaryHeap,
    time::{Duration, Instant},
};

use common::IPC_CHANNEL_BUFFER;
use store::{write::purge::PurgeStore, BlobStore, LookupStore, Store};
use tokio::sync::mpsc;
use utils::map::ttl_dashmap::TtlMap;

use crate::{Inner, JmapInstance, JMAP, LONG_SLUMBER};

pub enum Event {
    IndexStart,
    IndexDone,
    AcmeReload,
    AcmeReschedule {
        provider_id: String,
        renew_at: Instant,
    },
    Purge(PurgeType),
    #[cfg(feature = "test_mode")]
    IndexIsActive(tokio::sync::oneshot::Sender<bool>),
    Exit,
}

pub enum PurgeType {
    Data(Store),
    Blobs { store: Store, blob_store: BlobStore },
    Lookup(LookupStore),
    Account(Option<u32>),
}

#[derive(PartialEq, Eq)]
struct Action {
    due: Instant,
    event: ActionClass,
}

#[derive(PartialEq, Eq, Debug)]
enum ActionClass {
    Session,
    Account,
    Store(usize),
    Acme(String),
    #[cfg(feature = "enterprise")]
    ReloadLicense,
}

#[derive(Default)]
struct Queue {
    heap: BinaryHeap<Action>,
}

pub fn spawn_housekeeper(core: JmapInstance, mut rx: mpsc::Receiver<Event>) {
    tokio::spawn(async move {
        tracing::debug!("Housekeeper task started.");

        let mut index_busy = true;
        let mut index_pending = false;

        // Index any queued messages
        let jmap = JMAP::from(core.clone());
        tokio::spawn(async move {
            jmap.fts_index_queued().await;
        });

        // Add all events to queue
        let mut queue = Queue::default();
        {
            let core_ = core.core.load();
            queue.schedule(
                Instant::now() + core_.jmap.session_purge_frequency.time_to_next(),
                ActionClass::Session,
            );
            queue.schedule(
                Instant::now() + core_.jmap.account_purge_frequency.time_to_next(),
                ActionClass::Account,
            );
            for (idx, schedule) in core_.storage.purge_schedules.iter().enumerate() {
                queue.schedule(
                    Instant::now() + schedule.cron.time_to_next(),
                    ActionClass::Store(idx),
                );
            }

            // Add all ACME renewals to heap
            for provider in core_.tls.acme_providers.values() {
                match core_.init_acme(provider).await {
                    Ok(renew_at) => {
                        queue.schedule(
                            Instant::now() + renew_at,
                            ActionClass::Acme(provider.id.clone()),
                        );
                    }
                    Err(err) => {
                        tracing::error!(
                        context = "acme",
                        event = "error",
                        error = ?err,
                        "Failed to initialize ACME certificate manager.");
                    }
                };
            }

            // SPDX-SnippetBegin
            // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
            // SPDX-License-Identifier: LicenseRef-SEL

            // Enterprise Edition license management
            #[cfg(feature = "enterprise")]
            if let Some(enterprise) = &core_.enterprise {
                queue.schedule(
                    Instant::now() + enterprise.license.expires_in(),
                    ActionClass::ReloadLicense,
                );
            }
            // SPDX-SnippetEnd
        }

        loop {
            match tokio::time::timeout(queue.wake_up_time(), rx.recv()).await {
                Ok(Some(event)) => match event {
                    Event::AcmeReload => {
                        let core_ = core.core.load().clone();
                        let inner = core.jmap_inner.clone();

                        tokio::spawn(async move {
                            for provider in core_.tls.acme_providers.values() {
                                match core_.init_acme(provider).await {
                                    Ok(renew_at) => {
                                        inner
                                            .housekeeper_tx
                                            .send(Event::AcmeReschedule {
                                                provider_id: provider.id.clone(),
                                                renew_at: Instant::now() + renew_at,
                                            })
                                            .await
                                            .ok();
                                    }
                                    Err(err) => {
                                        tracing::error!(
                                            context = "acme",
                                            event = "error",
                                            error = ?err,
                                            "Failed to reload ACME certificate manager.");
                                    }
                                };
                            }
                        });
                    }
                    Event::AcmeReschedule {
                        provider_id,
                        renew_at,
                    } => {
                        let action = ActionClass::Acme(provider_id);
                        queue.remove_action(&action);
                        queue.schedule(renew_at, action);
                    }
                    Event::IndexStart => {
                        if !index_busy {
                            index_busy = true;
                            let jmap = JMAP::from(core.clone());
                            tokio::spawn(async move {
                                jmap.fts_index_queued().await;
                            });
                        } else {
                            index_pending = true;
                        }
                    }
                    Event::IndexDone => {
                        if index_pending {
                            index_pending = false;
                            let jmap = JMAP::from(core.clone());
                            tokio::spawn(async move {
                                jmap.fts_index_queued().await;
                            });
                        } else {
                            index_busy = false;
                        }
                    }
                    Event::Purge(purge) => match purge {
                        PurgeType::Data(store) => {
                            tokio::spawn(async move {
                                if let Err(err) = store.purge_store().await {
                                    tracing::error!("Failed to purge data store: {err}",);
                                }
                            });
                        }
                        PurgeType::Blobs { store, blob_store } => {
                            tokio::spawn(async move {
                                if let Err(err) = store.purge_blobs(blob_store).await {
                                    tracing::error!("Failed to purge blob store: {err}",);
                                }
                            });
                        }
                        PurgeType::Lookup(store) => {
                            tokio::spawn(async move {
                                if let Err(err) = store.purge_lookup_store().await {
                                    tracing::error!("Failed to purge lookup store: {err}",);
                                }
                            });
                        }
                        PurgeType::Account(account_id) => {
                            let jmap = JMAP::from(core.clone());
                            tokio::spawn(async move {
                                tracing::debug!("Purging accounts.");
                                if let Some(account_id) = account_id {
                                    jmap.purge_account(account_id).await;
                                } else {
                                    jmap.purge_accounts().await;
                                }
                            });
                        }
                    },
                    #[cfg(feature = "test_mode")]
                    Event::IndexIsActive(tx) => {
                        tx.send(index_busy).ok();
                    }
                    Event::Exit => {
                        tracing::debug!("Housekeeper task exiting.");
                        return;
                    }
                },
                Ok(None) => {
                    tracing::debug!("Housekeeper task exiting.");
                    return;
                }
                Err(_) => {
                    let core_ = core.core.load();
                    while let Some(event) = queue.pop() {
                        match event.event {
                            ActionClass::Acme(provider_id) => {
                                let inner = core.jmap_inner.clone();
                                let core = core_.clone();
                                tokio::spawn(async move {
                                    if let Some(provider) =
                                        core.tls.acme_providers.get(&provider_id)
                                    {
                                        tracing::info!(
                                            context = "acme",
                                            event = "order",
                                            domains = ?provider.domains,
                                            "Ordering certificates.");

                                        let renew_at = match core.renew(provider).await {
                                            Ok(renew_at) => {
                                                tracing::info!(
                                                    context = "acme",
                                                    event = "success",
                                                    domains = ?provider.domains,
                                                    next_renewal = ?renew_at,
                                                    "Certificates renewed.");
                                                renew_at
                                            }
                                            Err(err) => {
                                                tracing::error!(
                                                    context = "acme",
                                                    event = "error",
                                                    error = ?err,
                                                    "Failed to renew certificates.");

                                                Duration::from_secs(3600)
                                            }
                                        };

                                        inner.increment_config_version();

                                        inner
                                            .housekeeper_tx
                                            .send(Event::AcmeReschedule {
                                                provider_id: provider_id.clone(),
                                                renew_at: Instant::now() + renew_at,
                                            })
                                            .await
                                            .ok();
                                    }
                                });
                            }
                            ActionClass::Account => {
                                let jmap = JMAP::from(core.clone());
                                tokio::spawn(async move {
                                    tracing::debug!("Purging accounts.");
                                    jmap.purge_accounts().await;
                                });
                                queue.schedule(
                                    Instant::now()
                                        + core_.jmap.account_purge_frequency.time_to_next(),
                                    ActionClass::Account,
                                );
                            }
                            ActionClass::Session => {
                                let inner = core.jmap_inner.clone();
                                tokio::spawn(async move {
                                    tracing::debug!("Purging session cache.");
                                    inner.purge();
                                });
                                queue.schedule(
                                    Instant::now()
                                        + core_.jmap.session_purge_frequency.time_to_next(),
                                    ActionClass::Session,
                                );
                            }
                            ActionClass::Store(idx) => {
                                if let Some(schedule) =
                                    core_.storage.purge_schedules.get(idx).cloned()
                                {
                                    queue.schedule(
                                        Instant::now() + schedule.cron.time_to_next(),
                                        ActionClass::Store(idx),
                                    );
                                    tokio::spawn(async move {
                                        let (class, result) = match schedule.store {
                                            PurgeStore::Data(store) => {
                                                ("data", store.purge_store().await)
                                            }
                                            PurgeStore::Blobs { store, blob_store } => {
                                                ("blob", store.purge_blobs(blob_store).await)
                                            }
                                            PurgeStore::Lookup(lookup_store) => {
                                                ("lookup", lookup_store.purge_lookup_store().await)
                                            }
                                        };

                                        match result {
                                            Ok(_) => {
                                                tracing::debug!(
                                                    "Purged {class} store {}.",
                                                    schedule.store_id
                                                );
                                            }
                                            Err(err) => {
                                                tracing::error!(
                                                    "Failed to purge {class} store {}: {err}",
                                                    schedule.store_id
                                                );
                                            }
                                        }
                                    });
                                }
                            }

                            // SPDX-SnippetBegin
                            // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
                            // SPDX-License-Identifier: LicenseRef-SEL
                            #[cfg(feature = "enterprise")]
                            ActionClass::ReloadLicense => {
                                match core_.reload().await {
                                    Ok(result) => {
                                        if let Some(new_core) = result.new_core {
                                            if let Some(enterprise) = &new_core.enterprise {
                                                queue.schedule(
                                                    Instant::now()
                                                        + enterprise.license.expires_in(),
                                                    ActionClass::ReloadLicense,
                                                );
                                            }

                                            // Update core
                                            core.core.store(new_core.into());

                                            // Increment version counter
                                            core.jmap_inner.increment_config_version();
                                        }
                                    }
                                    Err(err) => {
                                        tracing::warn!("Failed to reload configuration: {err}",);
                                    }
                                }
                            } // SPDX-SnippetEnd
                        }
                    }
                }
            }
        }
    });
}

impl Queue {
    pub fn schedule(&mut self, due: Instant, event: ActionClass) {
        tracing::debug!(due_in = due.saturating_duration_since(Instant::now()).as_secs(), event = ?event, "Scheduling housekeeper event.");
        self.heap.push(Action { due, event });
    }

    pub fn remove_action(&mut self, event: &ActionClass) {
        self.heap.retain(|e| &e.event != event);
    }

    pub fn wake_up_time(&self) -> Duration {
        self.heap
            .peek()
            .map(|e| e.due.saturating_duration_since(Instant::now()))
            .unwrap_or(LONG_SLUMBER)
    }

    pub fn pop(&mut self) -> Option<Action> {
        if self.heap.peek()?.due <= Instant::now() {
            self.heap.pop()
        } else {
            None
        }
    }
}

impl Ord for Action {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.due.cmp(&other.due).reverse()
    }
}

impl PartialOrd for Action {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Inner {
    pub fn purge(&self) {
        self.sessions.cleanup();
        self.access_tokens.cleanup();
        self.concurrency_limiter
            .retain(|_, limiter| limiter.is_active());
    }
}

pub fn init_housekeeper() -> (mpsc::Sender<Event>, mpsc::Receiver<Event>) {
    mpsc::channel::<Event>(IPC_CHANNEL_BUFFER)
}
