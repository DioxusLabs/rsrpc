//! rrpc - Ergonomic Rust-to-Rust RPC
//!
//! A function-forward RPC library where the trait IS the API.
//!
//! # Example
//!
//! ```ignore
//! #[rrpc::service]
//! pub trait Worker: Send + Sync + 'static {
//!     async fn run_task(&self, task: Task) -> Result<Output, Error>;
//!     async fn status(&self) -> WorkerStatus;
//! }
//!
//! // Server
//! let server = serve_worker(my_worker);
//! server.listen("0.0.0.0:9000").await?;
//!
//! // Client - same method signatures!
//! let client: Client<dyn Worker> = Client::connect("10.0.0.5:9000").await?;
//! client.run_task(task).await?;
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};

/// Re-export the service macro
pub use rrpc_macro::service;

/// Re-exports for generated code
pub use async_trait::async_trait;
pub use postcard;
pub use serde;

/// Helper trait to normalize method return types for wire serialization.
/// Converts both `T` and `Result<T, E>` to `Result<T, String>`.
pub trait IntoWireResult<T> {
    fn into_wire_result(self) -> Result<T, String>;
}

impl<T, E: std::fmt::Display> IntoWireResult<T> for Result<T, E> {
    fn into_wire_result(self) -> Result<T, String> {
        self.map_err(|e| e.to_string())
    }
}

/// Wire format header size: method_id(2) + request_id(8) + payload_len(4) = 14 bytes
const HEADER_SIZE: usize = 14;

/// A client connection to a remote RPC server.
///
/// The magic: `Client<dyn MyTrait>` implements `MyTrait`, so you can call
/// `client.method(args)` directly.
pub struct Client<T: ?Sized> {
    inner: Arc<ClientInner>,
    _marker: PhantomData<T>,
}

struct ClientInner {
    writer: Mutex<tokio::io::WriteHalf<TcpStream>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Bytes>>>,
    next_request_id: AtomicU64,
}

impl<T: ?Sized> Clone for Client<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _marker: PhantomData,
        }
    }
}

impl<T: ?Sized + 'static> Client<T> {
    /// Connect to a remote RPC server over TCP.
    pub async fn connect(addr: &str) -> Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let (reader, writer) = tokio::io::split(stream);

        let inner = Arc::new(ClientInner {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
        });

        // Spawn reader task to handle responses
        let inner_clone = Arc::clone(&inner);
        tokio::spawn(async move {
            if let Err(e) = Self::read_responses(inner_clone, reader).await {
                eprintln!("Client reader error: {e}");
            }
        });

        Ok(Self {
            inner,
            _marker: PhantomData,
        })
    }

    async fn read_responses(
        inner: Arc<ClientInner>,
        mut reader: tokio::io::ReadHalf<TcpStream>,
    ) -> Result<()> {
        loop {
            // Read header
            let mut header = [0u8; HEADER_SIZE];
            if reader.read_exact(&mut header).await.is_err() {
                break; // Connection closed
            }

            let _method_id = u16::from_le_bytes([header[0], header[1]]);
            let request_id = u64::from_le_bytes([
                header[2], header[3], header[4], header[5], header[6], header[7], header[8],
                header[9],
            ]);
            let payload_len = u32::from_le_bytes([header[10], header[11], header[12], header[13]]);

            // Read payload
            let mut payload = vec![0u8; payload_len as usize];
            reader.read_exact(&mut payload).await?;

            // Dispatch to waiting caller - convert to Bytes for cheap cloning
            let sender = inner.pending.lock().await.remove(&request_id);
            if let Some(tx) = sender {
                let _ = tx.send(Bytes::from(payload));
            }
        }
        Ok(())
    }

    /// Low-level call method used by generated trait impls.
    /// Sends a request and waits for the response.
    pub async fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        method_id: u16,
        request: &Req,
    ) -> Result<Resp> {
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let payload = postcard::to_allocvec(request)?;

        // Register pending request
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(request_id, tx);

        // Build and send message
        let mut message = Vec::with_capacity(HEADER_SIZE + payload.len());
        message.extend_from_slice(&method_id.to_le_bytes());
        message.extend_from_slice(&request_id.to_le_bytes());
        message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        message.extend_from_slice(&payload);

        self.inner.writer.lock().await.write_all(&message).await?;

        // Wait for response
        let response_payload = rx.await.map_err(|_| anyhow!("Request cancelled"))?;
        let response: Result<Resp, String> = postcard::from_bytes(&response_payload)?;
        response.map_err(|e| anyhow!("{e}"))
    }
}

/// Type alias for dispatch handler functions.
/// Takes (service, method_id, payload) and returns serialized response.
pub type DispatchFn<T> = for<'a> fn(
    &'a T,
    u16,
    &'a [u8],
) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>>;

/// A server that hosts an RPC service implementation.
pub struct Server<T> {
    service: Arc<T>,
    dispatch: DispatchFn<T>,
}

impl<T: Send + Sync + 'static> Server<T> {
    /// Create a new server with the given service implementation and dispatch function.
    ///
    /// This is typically called by generated code from `#[rrpc::service]`.
    pub fn new(service: T, dispatch: DispatchFn<T>) -> Self {
        Self {
            service: Arc::new(service),
            dispatch,
        }
    }

    /// Listen for incoming connections on the given address.
    pub async fn listen(self, addr: &str) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        println!("Server listening on {addr}");

        loop {
            let (stream, peer) = listener.accept().await?;
            println!("New connection from {peer}");

            let service = Arc::clone(&self.service);
            let dispatch = self.dispatch;

            tokio::spawn(async move {
                if let Err(e) = Self::handle_connection(stream, service, dispatch).await {
                    eprintln!("Connection error: {e}");
                }
            });
        }
    }

    async fn handle_connection(
        stream: TcpStream,
        service: Arc<T>,
        dispatch: DispatchFn<T>,
    ) -> Result<()> {
        let (mut reader, mut writer) = tokio::io::split(stream);

        loop {
            // Read header
            let mut header = [0u8; HEADER_SIZE];
            if reader.read_exact(&mut header).await.is_err() {
                break; // Connection closed
            }

            let method_id = u16::from_le_bytes([header[0], header[1]]);
            let request_id = u64::from_le_bytes([
                header[2], header[3], header[4], header[5], header[6], header[7], header[8],
                header[9],
            ]);
            let payload_len = u32::from_le_bytes([header[10], header[11], header[12], header[13]]);

            // Read payload
            let mut payload = vec![0u8; payload_len as usize];
            reader.read_exact(&mut payload).await?;

            // Dispatch to handler
            let response_result = dispatch(&service, method_id, &payload).await;

            // Serialize response (wrap in Result for error propagation)
            let response_payload = match response_result {
                Ok(data) => data,
                Err(e) => postcard::to_allocvec(&Err::<(), _>(e.to_string()))?,
            };

            // Send response
            let mut response = Vec::with_capacity(HEADER_SIZE + response_payload.len());
            response.extend_from_slice(&method_id.to_le_bytes());
            response.extend_from_slice(&request_id.to_le_bytes());
            response.extend_from_slice(&(response_payload.len() as u32).to_le_bytes());
            response.extend_from_slice(&response_payload);

            writer.write_all(&response).await?;
        }

        Ok(())
    }
}
