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
//! let server = <dyn Worker>::serve(my_worker);
//! server.listen("0.0.0.0:9000").await?;
//!
//! // Client
//! let client: Client<dyn Worker> = Client::connect("10.0.0.5:9000").await?;
//! client.run_task(task).await?;
//! ```
//!
//! # Streaming
//!
//! Methods can return `RpcStream<T>` for server-side streaming:
//!
//! ```ignore
//! #[rrpc::service]
//! pub trait LogService: Send + Sync + 'static {
//!     async fn stream_logs(&self, filter: Filter) -> RpcStream<LogEntry>;
//! }
//! ```

mod stream;
pub use stream::*;

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
use tokio::sync::{mpsc, oneshot, Mutex};

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

/// A client connection to a remote RPC server.
///
/// The magic: `Client<dyn MyTrait>` implements `MyTrait`, so you can call
/// `client.method(args)` directly.
pub struct Client<T: ?Sized> {
    inner: Arc<ClientInner>,
    _marker: PhantomData<T>,
}

/// Internal state for pending requests
enum PendingRequest {
    /// Unary request waiting for single response
    Unary(oneshot::Sender<Bytes>),
    /// Streaming request receiving multiple items
    Stream(mpsc::Sender<StreamFrame>),
}

/// A frame received for a streaming response
pub struct StreamFrame {
    pub frame_type: FrameType,
    pub payload: Bytes,
}

struct ClientInner {
    writer: Mutex<tokio::io::WriteHalf<TcpStream>>,
    pending: Mutex<HashMap<u64, PendingRequest>>,
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
            // Read header (15 bytes with frame type)
            let mut header = [0u8; STREAM_HEADER_SIZE];
            if reader.read_exact(&mut header).await.is_err() {
                break; // Connection closed
            }

            let Some((frame_type, _method_id, request_id, payload_len)) =
                decode_stream_header(&header)
            else {
                eprintln!("Invalid frame type received");
                continue;
            };

            // Read payload
            let mut payload = vec![0u8; payload_len as usize];
            reader.read_exact(&mut payload).await?;
            let payload = Bytes::from(payload);

            // Dispatch based on request type
            let mut pending = inner.pending.lock().await;

            match frame_type {
                FrameType::Response => {
                    // Unary response - remove and complete
                    if let Some(PendingRequest::Unary(tx)) = pending.remove(&request_id) {
                        let _ = tx.send(payload);
                    }
                }
                FrameType::StreamItem => {
                    // Stream item - send to stream channel
                    if let Some(PendingRequest::Stream(tx)) = pending.get(&request_id) {
                        let _ = tx
                            .send(StreamFrame {
                                frame_type,
                                payload,
                            })
                            .await;
                    }
                }
                FrameType::StreamEnd | FrameType::StreamError => {
                    // Stream completed or errored - send final frame and remove
                    if let Some(PendingRequest::Stream(tx)) = pending.remove(&request_id) {
                        let _ = tx
                            .send(StreamFrame {
                                frame_type,
                                payload,
                            })
                            .await;
                    }
                }
                FrameType::Request => {
                    // Client shouldn't receive Request frames
                    eprintln!("Client received unexpected Request frame");
                }
            }
        }
        Ok(())
    }

    /// Low-level call method used by generated trait impls.
    /// Sends a request and waits for a unary response.
    pub async fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        method_id: u16,
        request: &Req,
    ) -> Result<Resp> {
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let payload = postcard::to_allocvec(request)?;

        // Register pending request
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(request_id, PendingRequest::Unary(tx));

        // Build and send message with frame type
        let header = encode_stream_header(
            FrameType::Request,
            method_id,
            request_id,
            payload.len() as u32,
        );
        let mut message = Vec::with_capacity(STREAM_HEADER_SIZE + payload.len());
        message.extend_from_slice(&header);
        message.extend_from_slice(&payload);

        self.inner.writer.lock().await.write_all(&message).await?;

        // Wait for response
        let response_payload = rx.await.map_err(|_| anyhow!("Request cancelled"))?;
        let response: Result<Resp, String> = postcard::from_bytes(&response_payload)?;
        response.map_err(|e| anyhow!("{e}"))
    }

    /// Start a streaming call. Returns a stream of responses.
    pub async fn call_stream<Req: Serialize, Item: DeserializeOwned + Send + 'static>(
        &self,
        method_id: u16,
        request: &Req,
    ) -> Result<RpcStream<Item>> {
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let payload = postcard::to_allocvec(request)?;

        // Create channels for stream
        let (frame_tx, mut frame_rx) = mpsc::channel::<StreamFrame>(32);
        let (item_tx, item_rx) = mpsc::channel::<Result<Item, String>>(32);

        // Register pending stream
        self.inner
            .pending
            .lock()
            .await
            .insert(request_id, PendingRequest::Stream(frame_tx));

        // Spawn task to convert frames to items
        tokio::spawn(async move {
            while let Some(frame) = frame_rx.recv().await {
                match frame.frame_type {
                    FrameType::StreamItem => {
                        match postcard::from_bytes::<Item>(&frame.payload) {
                            Ok(item) => {
                                if item_tx.send(Ok(item)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = item_tx.send(Err(e.to_string())).await;
                                break;
                            }
                        }
                    }
                    FrameType::StreamEnd => {
                        break;
                    }
                    FrameType::StreamError => {
                        let error: String =
                            postcard::from_bytes(&frame.payload).unwrap_or_else(|_| {
                                "Unknown stream error".to_string()
                            });
                        let _ = item_tx.send(Err(error)).await;
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Send request
        let header = encode_stream_header(
            FrameType::Request,
            method_id,
            request_id,
            payload.len() as u32,
        );
        let mut message = Vec::with_capacity(STREAM_HEADER_SIZE + payload.len());
        message.extend_from_slice(&header);
        message.extend_from_slice(&payload);

        self.inner.writer.lock().await.write_all(&message).await?;

        Ok(RpcStream::new(item_rx))
    }

    /// Get a handle for sending stream items to the server.
    /// Used for client-side streaming and bidirectional streaming.
    pub fn stream_sender(&self, method_id: u16, request_id: u64) -> ClientStreamSender {
        ClientStreamSender {
            inner: Arc::clone(&self.inner),
            method_id,
            request_id,
        }
    }

    /// Allocate a new request ID.
    pub fn next_request_id(&self) -> u64 {
        self.inner.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Access the inner writer for advanced use cases.
    pub fn writer(&self) -> &Mutex<tokio::io::WriteHalf<TcpStream>> {
        &self.inner.writer
    }
}

/// Handle for sending stream items from client to server.
pub struct ClientStreamSender {
    inner: Arc<ClientInner>,
    method_id: u16,
    request_id: u64,
}

impl ClientStreamSender {
    /// Send an item to the server stream.
    pub async fn send<T: Serialize>(&self, item: &T) -> Result<()> {
        let payload = postcard::to_allocvec(item)?;
        let header = encode_stream_header(
            FrameType::StreamItem,
            self.method_id,
            self.request_id,
            payload.len() as u32,
        );

        let mut message = Vec::with_capacity(STREAM_HEADER_SIZE + payload.len());
        message.extend_from_slice(&header);
        message.extend_from_slice(&payload);

        self.inner.writer.lock().await.write_all(&message).await?;
        Ok(())
    }

    /// Signal end of client stream.
    pub async fn end(&self) -> Result<()> {
        let header =
            encode_stream_header(FrameType::StreamEnd, self.method_id, self.request_id, 0);
        self.inner.writer.lock().await.write_all(&header).await?;
        Ok(())
    }

    /// Send an error and end the stream.
    pub async fn error(&self, err: impl std::fmt::Display) -> Result<()> {
        let payload = postcard::to_allocvec(&err.to_string())?;
        let header = encode_stream_header(
            FrameType::StreamError,
            self.method_id,
            self.request_id,
            payload.len() as u32,
        );

        let mut message = Vec::with_capacity(STREAM_HEADER_SIZE + payload.len());
        message.extend_from_slice(&header);
        message.extend_from_slice(&payload);

        self.inner.writer.lock().await.write_all(&message).await?;
        Ok(())
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
///
/// Use `<dyn MyTrait>::serve(impl)` to create a server.
pub struct Server<T: ?Sized> {
    service: Arc<T>,
    dispatch: DispatchFn<T>,
}

impl<T: ?Sized + Send + Sync + 'static> Server<T> {
    /// Create a server from an Arc'd service and dispatch function.
    /// Typically you should use `<dyn MyTrait>::serve(impl)` instead.
    pub fn from_arc(service: Arc<T>, dispatch: DispatchFn<T>) -> Self {
        Self { service, dispatch }
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
        let (mut reader, writer) = tokio::io::split(stream);
        let writer = Arc::new(Mutex::new(writer));

        loop {
            // Read header (15 bytes with frame type)
            let mut header = [0u8; STREAM_HEADER_SIZE];
            if reader.read_exact(&mut header).await.is_err() {
                break; // Connection closed
            }

            let Some((frame_type, method_id, request_id, payload_len)) =
                decode_stream_header(&header)
            else {
                eprintln!("Invalid frame type received");
                continue;
            };

            // Read payload
            let mut payload = vec![0u8; payload_len as usize];
            reader.read_exact(&mut payload).await?;

            match frame_type {
                FrameType::Request => {
                    // Unary request - dispatch and send response
                    let response_result = dispatch(&service, method_id, &payload).await;

                    // Serialize response
                    let response_payload = match response_result {
                        Ok(data) => data,
                        Err(e) => postcard::to_allocvec(&Err::<(), _>(e.to_string()))?,
                    };

                    // Send response with Response frame type
                    let response_header = encode_stream_header(
                        FrameType::Response,
                        method_id,
                        request_id,
                        response_payload.len() as u32,
                    );

                    let mut response =
                        Vec::with_capacity(STREAM_HEADER_SIZE + response_payload.len());
                    response.extend_from_slice(&response_header);
                    response.extend_from_slice(&response_payload);

                    writer.lock().await.write_all(&response).await?;
                }
                FrameType::StreamItem | FrameType::StreamEnd | FrameType::StreamError => {
                    // Client-side streaming frames - would need stream handler registration
                    // For now, these are handled by streaming dispatch handlers
                    eprintln!("Server received stream frame (not yet routed): {:?}", frame_type);
                }
                FrameType::Response => {
                    // Server shouldn't receive Response frames
                    eprintln!("Server received unexpected Response frame");
                }
            }
        }

        Ok(())
    }
}

/// Handle for sending stream responses from server to client.
pub struct ServerStreamSender {
    writer: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
    method_id: u16,
    request_id: u64,
}

impl ServerStreamSender {
    /// Create a new server stream sender.
    pub fn new(
        writer: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
        method_id: u16,
        request_id: u64,
    ) -> Self {
        Self {
            writer,
            method_id,
            request_id,
        }
    }

    /// Send an item to the client stream.
    pub async fn send<T: Serialize>(&self, item: &T) -> Result<()> {
        let payload = postcard::to_allocvec(item)?;
        let header = encode_stream_header(
            FrameType::StreamItem,
            self.method_id,
            self.request_id,
            payload.len() as u32,
        );

        let mut message = Vec::with_capacity(STREAM_HEADER_SIZE + payload.len());
        message.extend_from_slice(&header);
        message.extend_from_slice(&payload);

        self.writer.lock().await.write_all(&message).await?;
        Ok(())
    }

    /// Signal successful end of stream.
    pub async fn end(&self) -> Result<()> {
        let header =
            encode_stream_header(FrameType::StreamEnd, self.method_id, self.request_id, 0);
        self.writer.lock().await.write_all(&header).await?;
        Ok(())
    }

    /// Send an error and end the stream.
    pub async fn error(&self, err: impl std::fmt::Display) -> Result<()> {
        let payload = postcard::to_allocvec(&err.to_string())?;
        let header = encode_stream_header(
            FrameType::StreamError,
            self.method_id,
            self.request_id,
            payload.len() as u32,
        );

        let mut message = Vec::with_capacity(STREAM_HEADER_SIZE + payload.len());
        message.extend_from_slice(&header);
        message.extend_from_slice(&payload);

        self.writer.lock().await.write_all(&message).await?;
        Ok(())
    }
}
