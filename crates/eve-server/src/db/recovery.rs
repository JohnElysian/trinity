//! Replace failed connections at operation boundaries, never replay SQL.
//!
//! A lease pins one PostgreSQL session for the whole operation. In particular,
//! reconnecting cannot move an advisory lock, transaction, or a possibly
//! committed mutation onto a different session and silently execute it again.
use std::{sync::Arc, time::{Duration, Instant}};
use std::sync::RwLock;
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_postgres::{Client, Error};
#[cfg(test)] mod tests;

const CONNECT_LIMIT:Duration=Duration::from_secs(10);

#[derive(Debug,thiserror::Error)]
pub enum AcquireError {
    #[error(transparent)]
    Database(#[from] Error),
    #[error("database connection recovery timed out")]
    Timeout,
}

async fn bounded_connect(url:&str)->Result<Client,AcquireError> {
    tokio::time::timeout(CONNECT_LIMIT,super::connect(url)).await
        .map_err(|_|AcquireError::Timeout)?.map_err(Into::into)
}

pub struct Shared {
    url: String,
    current: RwLock<Arc<Client>>,
    connecting: Mutex<()>,
}

impl Shared {
    pub fn new(client: Client, url: impl Into<String>) -> Self {
        Self {url:url.into(),current:RwLock::new(Arc::new(client)),connecting:Mutex::new(())}
    }

    /// Seed a connection owner synchronously. It must call `lease` before its
    /// first operation; this snapshot can already be closed.
    pub fn snapshot(&self) -> Arc<Client> {self.current.read().unwrap_or_else(|e|e.into_inner()).clone()}

    pub async fn lease(&self) -> Result<Arc<Client>, AcquireError> {
        let current=self.snapshot();
        if !current.is_closed() {return Ok(current);}
        tokio::time::timeout(CONNECT_LIMIT,self.replace()).await.map_err(|_|AcquireError::Timeout)?
    }

    async fn replace(&self)->Result<Arc<Client>,AcquireError> {
        let _connecting=self.connecting.lock().await;
        let current=self.snapshot();
        if !current.is_closed() {return Ok(current);}
        let client=Arc::new(super::connect(&self.url).await?);
        *self.current.write().unwrap_or_else(|e|e.into_inner())=client.clone();
        tracing::info!("shared database connection recovered");
        Ok(client)
    }
}

/// Dedicated workers reconnect in the background so their simulation clocks
/// keep running. The caller retains work it has *not* dispatched; this helper
/// has no knowledge of SQL and never retries a failed operation.
pub struct Reconnect {
    attempt: Option<JoinHandle<Result<Client,AcquireError>>>,
    next: Instant,
}

impl Default for Reconnect {
    fn default()->Self {Self {attempt:None,next:Instant::now()}}
}

impl Reconnect {
    pub async fn poll(&mut self,url:&str)->Option<Client> {
        if self.attempt.as_ref().is_some_and(|attempt|attempt.is_finished()) {
            let result=self.attempt.take().unwrap().await;
            match result {
                Ok(Ok(client))=>{
                    tracing::info!("worker database connection recovered");
                    return Some(client);
                }
                Ok(Err(error))=>tracing::warn!("worker database reconnect failed: {}",crate::logging::chain(&error)),
                Err(error)=>tracing::warn!("worker database reconnect task failed: {error}"),
            }
            self.next=Instant::now()+Duration::from_secs(5);
        }
        if self.attempt.is_none() && Instant::now()>=self.next {
            let url=url.to_owned();
            self.attempt=Some(tokio::spawn(async move {bounded_connect(&url).await}));
        }
        None
    }
}

impl Drop for Reconnect {
    fn drop(&mut self) {if let Some(attempt)=self.attempt.take() {attempt.abort();}}
}
