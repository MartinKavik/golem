// Copyright 2024 Golem Cloud
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::config::RetryConfig;
use crate::retries::RetryState;
use dashmap::DashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Status};

#[derive(Clone)]
pub struct GrpcClient<T: Clone> {
    endpoint: http_02::Uri,
    config: GrpcClientConfig,
    client: Arc<Mutex<Option<GrpcClientConnection<T>>>>,
    client_factory: Arc<dyn Fn(Channel) -> T + Send + Sync + 'static>,
}

impl<T: Clone> GrpcClient<T> {
    pub fn new(
        client_factory: impl Fn(Channel) -> T + Send + Sync + 'static,
        endpoint: http_02::Uri,
        config: GrpcClientConfig,
    ) -> Self {
        Self {
            endpoint,
            config,
            client: Arc::new(Mutex::new(None)),
            client_factory: Arc::new(client_factory),
        }
    }

    pub async fn call<F, R>(&self, f: F) -> Result<R, Status>
    where
        F: for<'a> Fn(&'a mut T) -> Pin<Box<dyn Future<Output = Result<R, Status>> + 'a + Send>>
            + Send,
    {
        let mut retries = RetryState::new(&self.config.retries_on_unavailable);
        loop {
            retries.start_attempt();
            let mut entry = self
                .get()
                .await
                .map_err(|err| Status::from_error(Box::new(err)))?;
            match f(&mut entry.client).await {
                Ok(result) => break Ok(result),
                Err(e) => {
                    if requires_reconnect(&e) {
                        let _ = self.client.lock().await.take();
                        if !retries.failed_attempt().await {
                            break Err(e);
                        } else {
                            continue; // retry
                        }
                    } else {
                        break Err(e);
                    }
                }
            }
        }
    }

    async fn get(&self) -> Result<GrpcClientConnection<T>, tonic::transport::Error> {
        let mut entry = self.client.lock().await;

        match &*entry {
            Some(client) => Ok(client.clone()),
            None => {
                let endpoint = Endpoint::new(self.endpoint.clone())?
                    .connect_timeout(self.config.connect_timeout);
                let channel = endpoint.connect_lazy();
                let client = (self.client_factory)(channel);
                let connection = GrpcClientConnection { client };
                *entry = Some(connection.clone());
                Ok(connection)
            }
        }
    }
}

#[derive(Clone)]
pub struct MultiTargetGrpcClient<T: Clone> {
    config: GrpcClientConfig,
    clients: Arc<DashMap<http_02::Uri, GrpcClientConnection<T>>>,
    client_factory: Arc<dyn Fn(Channel) -> T + Send + Sync>,
}

impl<T: Clone> MultiTargetGrpcClient<T> {
    pub fn new(
        client_factory: impl Fn(Channel) -> T + Send + Sync + 'static,
        config: GrpcClientConfig,
    ) -> Self {
        Self {
            config,
            clients: Arc::new(DashMap::new()),
            client_factory: Arc::new(client_factory),
        }
    }

    pub async fn call<F, R>(&self, endpoint: http_02::Uri, f: F) -> Result<R, Status>
    where
        F: for<'a> Fn(&'a mut T) -> Pin<Box<dyn Future<Output = Result<R, Status>> + 'a + Send>>
            + Send,
    {
        let retries = RetryState::new(&self.config.retries_on_unavailable);
        loop {
            let mut entry = self
                .get(endpoint.clone())
                .map_err(|err| Status::from_error(Box::new(err)))?;
            match f(&mut entry.client).await {
                Ok(result) => break Ok(result),
                Err(e) => {
                    if requires_reconnect(&e) {
                        self.clients.remove(&endpoint);
                        if !retries.failed_attempt().await {
                            break Err(e);
                        } else {
                            continue; // retry
                        }
                    } else {
                        break Err(e);
                    }
                }
            }
        }
    }

    fn get(
        &self,
        endpoint: http_02::Uri,
    ) -> Result<GrpcClientConnection<T>, tonic::transport::Error> {
        let connect_timeout = self.config.connect_timeout;
        let entry = self
            .clients
            .entry(endpoint.clone())
            .or_try_insert_with(move || {
                let endpoint = Endpoint::new(endpoint)?.connect_timeout(connect_timeout);
                let channel = endpoint.connect_lazy();
                let client = (self.client_factory)(channel);
                Ok(GrpcClientConnection { client })
            })?;
        Ok(entry.clone())
    }
}

#[derive(Clone)]
pub struct GrpcClientConnection<T: Clone> {
    client: T,
}

#[derive(Debug, Clone)]
pub struct GrpcClientConfig {
    pub connect_timeout: Duration,
    pub retries_on_unavailable: RetryConfig,
}

impl Default for GrpcClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            retries_on_unavailable: RetryConfig::default(),
        }
    }
}

fn requires_reconnect(e: &Status) -> bool {
    e.code() == Code::Unavailable
}
