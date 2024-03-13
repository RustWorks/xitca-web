use core::{future::IntoFuture, sync::atomic::Ordering};

use std::sync::Arc;

use postgres_types::{BorrowToSql, ToSql, Type};
use tokio::sync::{Notify, RwLock, RwLockReadGuard};

use crate::{
    client::Client,
    config::Config,
    driver::connect,
    error::Error,
    iter::slice_iter,
    statement::{Statement, StatementGuarded},
    util::lock::Lock,
    RowSimpleStream, RowStream,
};

/// a shared connection for non transaction queries and [Statement] cache live as long as the connection itself.
pub struct SharedClient {
    inner: RwLock<Client>,
    persist: Box<Persist>,
}

struct Persist {
    spawner: Spawner,
    config: Config,
    statements_cache: Vec<(usize, String, Vec<Type>)>,
}

impl Persist {
    fn spawn_guard(&self) -> SpawnGuard<'_> {
        SpawnGuard(self)
    }
}

struct Spawner {
    notify: Lock<Option<Arc<Notify>>>,
}

impl Spawner {
    #[cold]
    #[inline(never)]
    fn spawn_or_wait(&self) -> Option<Arc<Notify>> {
        let mut lock = self.notify.lock();
        match *lock {
            Some(ref notify) => Some(notify.clone()),
            None => {
                *lock = Some(Arc::new(Notify::new()));
                None
            }
        }
    }

    #[cold]
    #[inline(never)]
    async fn wait_for_spawn(&self) {
        let notify = self.notify.lock().clone();
        if let Some(notify) = notify {
            notify.notified().await;
        }
    }
}

struct SpawnGuard<'a>(&'a Persist);

impl Drop for SpawnGuard<'_> {
    fn drop(&mut self) {
        // if for any reason current task is cancelled by user the drop guard would
        // restore the spawning state.
        if let Some(notify) = self.0.spawner.notify.lock().take() {
            notify.notify_waiters();
        }
    }
}

impl SpawnGuard<'_> {
    #[cold]
    #[inline(never)]
    async fn spawn(&self) -> Client {
        let (cli, drv) = {
            loop {
                match connect(&mut self.0.config.clone()).await {
                    Ok(res) => break res,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
                }
            }
        };

        tokio::task::spawn(drv.into_future());

        for (id, query, types) in self.0.statements_cache.iter() {
            let _ = cli.prepare_with_id(*id, query.as_str(), types.as_slice()).await;
        }

        cli
    }
}

impl SharedClient {
    pub async fn new<C>(config: C) -> Result<Self, Error>
    where
        Config: TryFrom<C>,
        Error: From<<Config as TryFrom<C>>::Error>,
    {
        let mut config = Config::try_from(config)?;
        let (cli, drv) = connect(&mut config).await?;

        tokio::task::spawn(drv.into_future());

        Ok(Self {
            inner: RwLock::new(cli),
            persist: Box::new(Persist {
                spawner: Spawner {
                    notify: Lock::new(None),
                },
                config,
                statements_cache: Vec::new(),
            }),
        })
    }

    #[inline]
    pub async fn query<'a>(&self, stmt: &'a Statement, params: &[&(dyn ToSql + Sync)]) -> Result<RowStream<'a>, Error> {
        self.query_raw(stmt, slice_iter(params)).await
    }

    pub async fn query_raw<'a, I>(&self, stmt: &'a Statement, params: I) -> Result<RowStream<'a>, Error>
    where
        I: IntoIterator,
        I::IntoIter: ExactSizeIterator,
        I::Item: BorrowToSql,
    {
        let cli = self.read().await;
        match cli.query_raw(stmt, params).await {
            Err(Error::DriverDown(msg)) => {
                drop(cli);
                Box::pin(async move {
                    self.reconnect().await;
                    self.inner.read().await.query_buf(stmt, msg).await
                })
                .await
            }
            res => res,
        }
    }

    pub async fn query_simple(&self, stmt: &str) -> Result<RowSimpleStream, Error> {
        let cli = self.read().await;
        match cli.query_simple(stmt).await {
            Err(Error::DriverDown(msg)) => {
                drop(cli);
                Box::pin(async move {
                    self.reconnect().await;
                    self.inner.read().await.query_buf_simple(msg).await
                })
                .await
            }
            res => res,
        }
    }

    pub async fn prepare(
        &self,
        query: &str,
        types: &[Type],
    ) -> Result<StatementGuarded<RwLockReadGuard<'_, Client>>, Error> {
        loop {
            let cli = self.read().await;
            match cli._prepare(query, types).await {
                Ok(stmt) => return Ok(stmt.into_guarded(cli)),
                Err(Error::DriverDown(_)) => {
                    drop(cli);
                    Box::pin(self.reconnect()).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// cached statement that would live as long as SharedClient.
    pub async fn prepare_cached(&mut self, query: &str, types: &[Type]) -> Result<Statement, Error> {
        let cli = self.inner.read().await;
        let id = crate::prepare::NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let stmt = cli.prepare_with_id(id, query, types).await?;

        self.persist
            .statements_cache
            .push((id, String::from(query), types.into()));

        Ok(stmt)
    }

    #[cold]
    #[inline(never)]
    async fn reconnect(&self) {
        match self.persist.spawner.spawn_or_wait() {
            Some(wait) => wait.notified().await,
            None => {
                let guard = self.persist.spawn_guard();

                let mut cli = self.inner.write().await;

                *cli = guard.spawn().await;

                // release rwlock before spawn guard. when waiters are notified it's important that the lock
                // is free for read lock.
                drop(cli);
                drop(guard);
            }
        }
    }

    async fn read(&self) -> RwLockReadGuard<'_, Client> {
        loop {
            match self.inner.try_read() {
                Ok(cli) => return cli,
                // failing to acquire read lock means certain task is spawning new connection.
                // if there is no notify existing in spawner it means the spawn process has finished(or cancelled).
                // in that case just try read lock again.
                Err(_) => self.persist.spawner.wait_for_spawn().await,
            }
        }
    }
}

#[cfg(not(feature = "quic"))]
impl SharedClient {
    pub async fn pipeline<'a, const SYNC_MODE: bool>(
        &self,
        mut pipe: crate::pipeline::Pipeline<'a, SYNC_MODE>,
    ) -> Result<crate::pipeline::PipelineStream<'a>, Error> {
        let cli = self.read().await;
        match cli._pipeline::<SYNC_MODE>(&pipe.columns, pipe.buf).await {
            Ok(res) => Ok(crate::pipeline::PipelineStream {
                res,
                columns: pipe.columns,
                ranges: Vec::new(),
            }),
            Err(Error::DriverDown(buf)) => {
                drop(cli);
                pipe.buf = buf;
                Box::pin(self.pipeline_slow::<SYNC_MODE>(pipe)).await
            }
            Err(e) => Err(e),
        }
    }

    async fn pipeline_slow<'a, const SYNC_MODE: bool>(
        &self,
        mut pipe: crate::pipeline::Pipeline<'a, SYNC_MODE>,
    ) -> Result<crate::pipeline::PipelineStream<'a>, Error> {
        loop {
            self.reconnect().await;
            match self
                .read()
                .await
                ._pipeline_no_additive_sync::<SYNC_MODE>(&pipe.columns, pipe.buf)
                .await
            {
                Ok(res) => {
                    return Ok(crate::pipeline::PipelineStream {
                        res,
                        columns: pipe.columns,
                        ranges: Vec::new(),
                    })
                }
                Err(Error::DriverDown(buf)) => {
                    pipe.buf = buf;
                }
                Err(e) => return Err(e),
            }
        }
    }
}
