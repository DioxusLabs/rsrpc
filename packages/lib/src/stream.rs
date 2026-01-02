//! Streaming support for rsrpc.
//!
//! This module provides bidirectional streaming over RPC connections.

use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use bytes::Bytes;
use futures_core::Stream;
use serde::Serialize;
use tokio::sync::mpsc;

/// Frame types for the wire protocol.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// Unary request (client -> server)
    Request = 0,
    /// Unary response (server -> client)
    Response = 1,
    /// Stream data item (either direction)
    StreamItem = 2,
    /// Stream completed successfully (either direction)
    StreamEnd = 3,
    /// Stream error (either direction)
    StreamError = 4,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Request),
            1 => Some(Self::Response),
            2 => Some(Self::StreamItem),
            3 => Some(Self::StreamEnd),
            4 => Some(Self::StreamError),
            _ => None,
        }
    }
}

/// A bidirectional RPC stream.
///
/// `RpcStream` allows sending and receiving multiple items over a single
/// RPC call. It implements `Stream` for receiving items.
///
/// # Example
///
/// ```ignore
/// // Server streaming
/// async fn get_logs(&self, filter: Filter) -> RpcStream<LogEntry>;
///
/// // Client usage
/// let mut stream = client.get_logs(filter).await?;
/// while let Some(entry) = stream.next().await {
///     println!("{:?}", entry?);
/// }
/// ```
pub struct RpcStream<T> {
    rx: mpsc::Receiver<Result<T, String>>,
    tx: Option<StreamSender<T>>,
    _marker: std::marker::PhantomData<T>,
}

/// Handle for sending items into a stream.
#[derive(Clone)]
pub struct StreamSender<T> {
    inner: mpsc::Sender<Bytes>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Serialize> StreamSender<T> {
    /// Send an item into the stream.
    pub async fn send(&self, item: T) -> Result<()> {
        let bytes = crate::postcard::to_allocvec(&item)?;
        self.inner
            .send(Bytes::from(bytes))
            .await
            .map_err(|_| anyhow::anyhow!("Stream closed"))
    }

    /// Send an error and close the stream.
    pub async fn send_error(&self, error: String) -> Result<()> {
        let bytes = crate::postcard::to_allocvec(&Err::<(), _>(error))?;
        self.inner
            .send(Bytes::from(bytes))
            .await
            .map_err(|_| anyhow::anyhow!("Stream closed"))
    }
}

impl<T> RpcStream<T> {
    /// Create a new stream with the given receiver.
    pub fn new(rx: mpsc::Receiver<Result<T, String>>) -> Self {
        Self {
            rx,
            tx: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// Create a bidirectional stream with both send and receive capabilities.
    pub fn bidirectional(rx: mpsc::Receiver<Result<T, String>>, tx: mpsc::Sender<Bytes>) -> Self {
        Self {
            rx,
            tx: Some(StreamSender {
                inner: tx,
                _marker: std::marker::PhantomData,
            }),
            _marker: std::marker::PhantomData,
        }
    }

    /// Get the sender half for bidirectional streams.
    pub fn sender(&self) -> Option<&StreamSender<T>> {
        self.tx.as_ref()
    }

    /// Receive the next item from the stream.
    pub async fn next(&mut self) -> Option<Result<T, String>> {
        self.rx.recv().await
    }
}

impl<T: Unpin> Stream for RpcStream<T> {
    type Item = Result<T, String>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.rx).poll_recv(cx)
    }
}

/// Builder for creating streams on the server side.
pub struct StreamBuilder<T> {
    tx: mpsc::Sender<Result<T, String>>,
}

impl<T> StreamBuilder<T> {
    /// Create a new stream builder with the specified buffer size.
    pub fn new(buffer_size: usize) -> (Self, RpcStream<T>) {
        let (tx, rx) = mpsc::channel(buffer_size);
        (Self { tx }, RpcStream::new(rx))
    }

    /// Send an item to the stream.
    pub async fn send(&self, item: T) -> Result<()> {
        self.tx
            .send(Ok(item))
            .await
            .map_err(|_| anyhow::anyhow!("Stream receiver dropped"))
    }

    /// Send an error to the stream.
    pub async fn error(&self, err: impl std::fmt::Display) -> Result<()> {
        self.tx
            .send(Err(err.to_string()))
            .await
            .map_err(|_| anyhow::anyhow!("Stream receiver dropped"))
    }

    /// Get a clone of the sender for use in spawned tasks.
    pub fn sender(&self) -> mpsc::Sender<Result<T, String>> {
        self.tx.clone()
    }
}

impl<T> Clone for StreamBuilder<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

/// Wire format for streaming frames.
/// Header: [frame_type: u8][method_id: u16][request_id: u64][payload_len: u32] = 15 bytes
pub const STREAM_HEADER_SIZE: usize = 15;

/// Encode a streaming frame header.
pub fn encode_stream_header(
    frame_type: FrameType,
    method_id: u16,
    request_id: u64,
    payload_len: u32,
) -> [u8; STREAM_HEADER_SIZE] {
    let mut header = [0u8; STREAM_HEADER_SIZE];
    header[0] = frame_type as u8;
    header[1..3].copy_from_slice(&method_id.to_le_bytes());
    header[3..11].copy_from_slice(&request_id.to_le_bytes());
    header[11..15].copy_from_slice(&payload_len.to_le_bytes());
    header
}

/// Decode a streaming frame header.
pub fn decode_stream_header(
    header: &[u8; STREAM_HEADER_SIZE],
) -> Option<(FrameType, u16, u64, u32)> {
    let frame_type = FrameType::from_u8(header[0])?;
    let method_id = u16::from_le_bytes([header[1], header[2]]);
    let request_id = u64::from_le_bytes([
        header[3], header[4], header[5], header[6], header[7], header[8], header[9], header[10],
    ]);
    let payload_len = u32::from_le_bytes([header[11], header[12], header[13], header[14]]);
    Some((frame_type, method_id, request_id, payload_len))
}
