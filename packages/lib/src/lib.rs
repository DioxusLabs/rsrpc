//! rsrpc - Ergonomic Rust-to-Rust RPC
//!
//! A function-forward RPC library where the trait IS the API.
//!
//! # Example
//!
//! ```ignore
//! #[rsrpc::service]
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
//! Methods returning `Result<RpcStream<T>>` automatically stream data:
//!
//! ```ignore
//! #[rsrpc::service]
//! pub trait LogService: Send + Sync + 'static {
//!     async fn stream_logs(&self, filter: Filter) -> Result<RpcStream<LogEntry>>;
//! }
//! ```

mod stream;
pub use stream::*;

#[cfg(feature = "http")]
mod http_client;
#[cfg(feature = "http")]
pub use http_client::HttpClient;

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
pub use rsrpc_macro::service;

/// Re-exports for generated code
pub use async_trait::async_trait;
pub use postcard;
pub use serde;

/// Re-exports for HTTP support (only with `http` feature)
#[cfg(feature = "http")]
pub use ::http;
#[cfg(feature = "http")]
pub use axum;

// =============================================================================
// ENCODING TRAITS
// =============================================================================

/// Trait for encoding server responses into dispatch results.
///
/// This trait is automatically implemented for:
/// - `Result<T, E>` where `T: Serialize` - unary responses
/// - `Result<RpcStream<T>, E>` - streaming responses
///
/// The implementations don't conflict because `RpcStream<T>` intentionally
/// does not implement `Serialize`.
pub trait ServerEncoding {
    /// Convert this response into a dispatch result.
    fn into_dispatch(self) -> DispatchResult;
}

/// Unary response encoding for any serializable Result type.
impl<T: Serialize, E: std::fmt::Display> ServerEncoding for Result<T, E> {
    fn into_dispatch(self) -> DispatchResult {
        let wire_result: Result<T, String> = self.map_err(|e| e.to_string());
        match postcard::to_allocvec(&wire_result) {
            Ok(bytes) => DispatchResult::Unary(bytes),
            Err(e) => DispatchResult::Error(e.to_string()),
        }
    }
}

/// Streaming response encoding for RpcStream results.
/// This impl doesn't conflict with the above because RpcStream doesn't impl Serialize.
impl<T: Serialize + Unpin + Send + 'static, E: std::fmt::Display> ServerEncoding
    for Result<RpcStream<T>, E>
{
    fn into_dispatch(self) -> DispatchResult {
        match self {
            Ok(stream) => DispatchResult::Stream(Box::new(stream)),
            Err(e) => DispatchResult::Error(e.to_string()),
        }
    }
}

/// Trait for making client calls with automatic encoding/decoding.
///
/// This trait is automatically implemented for:
/// - `Result<T, anyhow::Error>` where `T: DeserializeOwned` - unary calls
/// - `Result<RpcStream<T>, anyhow::Error>` - streaming calls
pub trait ClientEncoding<Service: ?Sized + 'static>: Sized {
    /// Invoke a remote method and decode the response.
    fn invoke<R: Serialize + Sync>(
        client: &Client<Service>,
        method_id: u16,
        request: &R,
    ) -> impl Future<Output = Self> + Send;
}

/// Unary call encoding for any deserializable Result type.
impl<S: ?Sized + Sync + 'static, T: DeserializeOwned + Send> ClientEncoding<S>
    for Result<T, anyhow::Error>
{
    async fn invoke<R: Serialize + Sync>(client: &Client<S>, method_id: u16, request: &R) -> Self {
        client.call(method_id, request).await
    }
}

/// Streaming call encoding for RpcStream results.
/// This impl doesn't conflict with the above because RpcStream doesn't impl DeserializeOwned.
impl<S: ?Sized + Sync + 'static, T: DeserializeOwned + Send + 'static> ClientEncoding<S>
    for Result<RpcStream<T>, anyhow::Error>
{
    async fn invoke<R: Serialize + Sync>(client: &Client<S>, method_id: u16, request: &R) -> Self {
        client.call_stream(method_id, request).await
    }
}

// =============================================================================
// DISPATCH RESULT
// =============================================================================

/// Result from dispatching a method call.
/// Can be either a unary response or a stream of items.
pub enum DispatchResult {
    /// Unary response - single serialized payload
    Unary(Vec<u8>),
    /// Streaming response - boxed stream that yields serialized items
    Stream(Box<dyn ErasedStream + Send>),
    /// Error during dispatch
    Error(String),
}

/// Type-erased stream trait for dispatch results.
pub trait ErasedStream {
    /// Get the next item as serialized bytes.
    fn poll_next_bytes(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Vec<u8>, String>>>;
}

impl<T: Serialize + Unpin> ErasedStream for RpcStream<T> {
    fn poll_next_bytes(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Vec<u8>, String>>> {
        use futures_core::Stream;
        use std::task::Poll;

        match Stream::poll_next(self, cx) {
            Poll::Ready(Some(Ok(item))) => match postcard::to_allocvec(&item) {
                Ok(bytes) => Poll::Ready(Some(Ok(bytes))),
                Err(e) => Poll::Ready(Some(Err(e.to_string()))),
            },
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// =============================================================================
// CLIENT
// =============================================================================

/// Handle to the background reader task.
/// When dropped, aborts the reader task to allow clean process exit.
struct ReaderHandle(tokio::task::JoinHandle<()>);

impl Drop for ReaderHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A client connection to a remote RPC server.
///
/// The magic: `Client<dyn MyTrait>` implements `MyTrait`, so you can call
/// `client.method(args)` directly.
pub struct Client<T: ?Sized> {
    inner: Arc<ClientInner>,
    /// Keeps reader task alive while client is in use.
    /// When all Client clones are dropped, this aborts the reader.
    _reader: Arc<ReaderHandle>,
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
            _reader: Arc::clone(&self._reader),
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
        let reader_handle = tokio::spawn(async move {
            if let Err(e) = Self::read_responses(inner_clone, reader).await {
                // Only log if it's not a cancellation (which happens on clean shutdown)
                if !e.to_string().contains("canceled") {
                    eprintln!("Client reader error: {e}");
                }
            }
        });

        Ok(Self {
            inner,
            _reader: Arc::new(ReaderHandle(reader_handle)),
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
    pub async fn call<Req: Serialize + Sync, Resp: DeserializeOwned>(
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
    pub async fn call_stream<Req: Serialize + Sync, Item: DeserializeOwned + Send + 'static>(
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
                    FrameType::StreamItem => match postcard::from_bytes::<Item>(&frame.payload) {
                        Ok(item) => {
                            if item_tx.send(Ok(item)).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = item_tx.send(Err(e.to_string())).await;
                            break;
                        }
                    },
                    FrameType::StreamEnd => {
                        break;
                    }
                    FrameType::StreamError => {
                        let error: String = postcard::from_bytes(&frame.payload)
                            .unwrap_or_else(|_| "Unknown stream error".to_string());
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
}

// =============================================================================
// SERVER
// =============================================================================

/// Type alias for dispatch handler functions.
/// Takes (service, method_id, payload) and returns either unary or streaming result.
pub type DispatchFn<T> =
    for<'a> fn(&'a T, u16, &'a [u8]) -> Pin<Box<dyn Future<Output = DispatchResult> + Send + 'a>>;

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
                    let dispatch_result = dispatch(&service, method_id, &payload).await;
                    let writer = Arc::clone(&writer);

                    match dispatch_result {
                        DispatchResult::Unary(response_payload) => {
                            // Send unary response
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
                        DispatchResult::Stream(stream) => {
                            // Spawn task to send stream items
                            tokio::spawn(async move {
                                use std::future::poll_fn;
                                use std::pin::Pin;

                                let mut stream = stream;

                                loop {
                                    let item = poll_fn(|cx| {
                                        // SAFETY: The stream is boxed and we never move it
                                        let pinned = unsafe { Pin::new_unchecked(&mut *stream) };
                                        pinned.poll_next_bytes(cx)
                                    })
                                    .await;

                                    match item {
                                        Some(Ok(item_bytes)) => {
                                            let header = encode_stream_header(
                                                FrameType::StreamItem,
                                                method_id,
                                                request_id,
                                                item_bytes.len() as u32,
                                            );

                                            let mut message = Vec::with_capacity(
                                                STREAM_HEADER_SIZE + item_bytes.len(),
                                            );
                                            message.extend_from_slice(&header);
                                            message.extend_from_slice(&item_bytes);

                                            if writer
                                                .lock()
                                                .await
                                                .write_all(&message)
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Some(Err(e)) => {
                                            // Send error and end stream
                                            let error_bytes =
                                                postcard::to_allocvec(&e).unwrap_or_default();
                                            let header = encode_stream_header(
                                                FrameType::StreamError,
                                                method_id,
                                                request_id,
                                                error_bytes.len() as u32,
                                            );

                                            let mut message = Vec::with_capacity(
                                                STREAM_HEADER_SIZE + error_bytes.len(),
                                            );
                                            message.extend_from_slice(&header);
                                            message.extend_from_slice(&error_bytes);

                                            let _ = writer.lock().await.write_all(&message).await;
                                            break;
                                        }
                                        None => {
                                            // Stream ended - send StreamEnd
                                            let header = encode_stream_header(
                                                FrameType::StreamEnd,
                                                method_id,
                                                request_id,
                                                0,
                                            );

                                            let _ = writer.lock().await.write_all(&header).await;
                                            break;
                                        }
                                    }
                                }
                            });
                        }
                        DispatchResult::Error(e) => {
                            // Send error as unary response
                            let response_payload =
                                postcard::to_allocvec(&Err::<(), _>(e.to_string()))?;
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
                    }
                }
                FrameType::StreamItem | FrameType::StreamEnd | FrameType::StreamError => {
                    // Client-side streaming frames - would need stream handler registration
                    eprintln!(
                        "Server received stream frame (not yet routed): {:?}",
                        frame_type
                    );
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
