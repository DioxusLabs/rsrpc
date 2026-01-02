//! Streaming example for rrpc
//!
//! Demonstrates server-side streaming where the server sends multiple
//! log entries to the client over time.
//!
//! Run the server: cargo run --example streaming -- server
//! Run the client: cargo run --example streaming -- client

use anyhow::Result;
use rrpc::{async_trait, Client, RpcStream, StreamBuilder};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// =============================================================================
// STREAMING SERVICE DEFINITION
// =============================================================================

/// A log entry from the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: u64,
    pub level: String,
    pub message: String,
}

/// Filter for log streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogFilter {
    pub min_level: String,
    pub limit: Option<usize>,
}

/// A log streaming service.
///
/// Note: For now, streaming methods need manual implementation on both sides.
/// The macro generates unary call support; streaming is opt-in.
#[rrpc::service]
pub trait LogService: Send + Sync + 'static {
    /// Get server info (unary call).
    async fn get_info(&self) -> Result<String>;

    /// Get total log count (unary call).
    async fn get_log_count(&self) -> Result<u64>;
}

// =============================================================================
// SERVER IMPLEMENTATION
// =============================================================================

pub struct LogServer {
    logs: Vec<LogEntry>,
}

impl LogServer {
    pub fn new() -> Self {
        // Pre-populate with some fake logs
        let logs = vec![
            LogEntry {
                timestamp: 1000,
                level: "INFO".into(),
                message: "Server started".into(),
            },
            LogEntry {
                timestamp: 1001,
                level: "DEBUG".into(),
                message: "Loading configuration".into(),
            },
            LogEntry {
                timestamp: 1002,
                level: "INFO".into(),
                message: "Configuration loaded".into(),
            },
            LogEntry {
                timestamp: 1003,
                level: "WARN".into(),
                message: "Cache miss for key 'user:123'".into(),
            },
            LogEntry {
                timestamp: 1004,
                level: "ERROR".into(),
                message: "Connection timeout to database".into(),
            },
            LogEntry {
                timestamp: 1005,
                level: "INFO".into(),
                message: "Retrying database connection".into(),
            },
            LogEntry {
                timestamp: 1006,
                level: "INFO".into(),
                message: "Database connection established".into(),
            },
            LogEntry {
                timestamp: 1007,
                level: "DEBUG".into(),
                message: "Processing request #1".into(),
            },
            LogEntry {
                timestamp: 1008,
                level: "DEBUG".into(),
                message: "Processing request #2".into(),
            },
            LogEntry {
                timestamp: 1009,
                level: "INFO".into(),
                message: "Health check passed".into(),
            },
        ];
        Self { logs }
    }

    /// Stream logs to a client using StreamBuilder.
    /// This is called manually since streaming isn't auto-generated yet.
    pub async fn stream_logs(&self, filter: LogFilter) -> RpcStream<LogEntry> {
        let (builder, stream) = StreamBuilder::<LogEntry>::new(16);

        // Filter logs
        let filtered: Vec<_> = self
            .logs
            .iter()
            .filter(|log| match filter.min_level.as_str() {
                "DEBUG" => true,
                "INFO" => log.level != "DEBUG",
                "WARN" => log.level == "WARN" || log.level == "ERROR",
                "ERROR" => log.level == "ERROR",
                _ => true,
            })
            .take(filter.limit.unwrap_or(usize::MAX))
            .cloned()
            .collect();

        // Spawn task to send logs with delays (simulating real-time streaming)
        tokio::spawn(async move {
            for log in filtered {
                if builder.send(log).await.is_err() {
                    break;
                }
                // Simulate delay between log entries
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            // Stream ends when builder is dropped
        });

        stream
    }
}

#[async_trait]
impl LogService for LogServer {
    async fn get_info(&self) -> Result<String> {
        Ok("LogService v1.0 - Streaming enabled".into())
    }

    async fn get_log_count(&self) -> Result<u64> {
        Ok(self.logs.len() as u64)
    }
}

// =============================================================================
// MAIN
// =============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("server");
    let port = std::env::var("PORT").unwrap_or_else(|_| "8081".into());

    match mode {
        "server" => {
            let server = LogServer::new();

            // Test streaming locally first
            println!("Testing local streaming:");
            let filter = LogFilter {
                min_level: "INFO".into(),
                limit: Some(5),
            };
            let mut stream = server.stream_logs(filter).await;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(entry) => println!("  [{:>5}] {}", entry.level, entry.message),
                    Err(e) => eprintln!("  Error: {}", e),
                }
            }
            println!();

            // Start the RPC server
            println!("Starting server on 127.0.0.1:{port}...");
            println!("Note: Full streaming over RPC requires additional dispatch support.");
            println!("This example shows the streaming primitives in action.\n");

            let rpc_server = <dyn LogService>::serve(server);
            rpc_server.listen(&format!("127.0.0.1:{port}")).await?;
        }
        "client" => {
            println!("Connecting to server...");
            let client: Client<dyn LogService> =
                Client::connect(&format!("127.0.0.1:{port}")).await?;

            // Test unary calls
            let info = client.get_info().await?;
            println!("Server info: {}", info);

            let count = client.get_log_count().await?;
            println!("Total logs on server: {}", count);

            println!("\nNote: Full streaming over RPC is demonstrated in the server.");
            println!("The client can receive streams once dispatch is extended.");
        }
        _ => {
            println!("Usage: cargo run --example streaming -- [server|client]");
        }
    }

    Ok(())
}
