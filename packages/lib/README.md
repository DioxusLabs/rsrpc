# rsrpc

[![Crates.io](https://img.shields.io/crates/v/rsrpc.svg)](https://crates.io/crates/rsrpc)
[![Documentation](https://docs.rs/rsrpc/badge.svg)](https://docs.rs/rsrpc)
[![CI](https://github.com/dioxuslabs/rsrpc/actions/workflows/ci.yml/badge.svg)](https://github.com/dioxuslabs/rsrpc/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/dioxuslabs/rsrpc/blob/main/LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

Ergonomic Rust-to-Rust RPC where the trait is the API.

## Overview

rsrpc generates RPC client and server code from a trait definition. The client implements the same trait as the server, so `client.method(args)` just works. No separate client types, no message enums, no schema files.

```rust
#[rsrpc::service]
pub trait Worker: Send + Sync + 'static {
    async fn run_task(&self, task: Task) -> Result<Output>;
    async fn status(&self) -> Result<WorkerStatus>;
}
```

The macro generates:
- `impl Worker for Client<dyn Worker>` - call methods directly on the client
- `<dyn Worker>::serve(impl)` - wrap any implementation in a server

## Quick Start

```rust
use anyhow::Result;
use rsrpc::{async_trait, Client};

#[rsrpc::service]
pub trait Calculator: Send + Sync + 'static {
    async fn add(&self, a: i32, b: i32) -> Result<i32>;
}

// Server implementation
struct MyCalculator;

#[async_trait]
impl Calculator for MyCalculator {
    async fn add(&self, a: i32, b: i32) -> Result<i32> {
        Ok(a + b)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Server
    let server = <dyn Calculator>::serve(MyCalculator);
    tokio::spawn(server.listen("0.0.0.0:9000"));

    // Client
    let client: Client<dyn Calculator> = Client::connect("127.0.0.1:9000").await?;
    let result = client.add(2, 3).await?;
    assert_eq!(result, 5);
    Ok(())
}
```

## Features

- **Trait-based API**: Define your service as a Rust trait
- **Type-safe**: Full compile-time type checking for all RPC calls
- **Streaming**: Methods returning `Result<RpcStream<T>>` automatically stream
- **HTTP/REST**: Annotate methods with `#[get]`, `#[post]`, etc. for HTTP endpoints
- **Polymorphic**: Generic code works with both local and remote implementations

## HTTP/REST Support

Enable the `http` feature for REST endpoint support:

```rust
#[rsrpc::service]
pub trait UserService: Send + Sync + 'static {
    #[get("/users/{id}")]
    async fn get_user(&self, id: String) -> Result<User>;

    #[post("/users")]
    async fn create_user(&self, user: CreateUserRequest) -> Result<User>;
}

// Serve via HTTP
let router = <dyn UserService>::http_routes(service);
axum::serve(listener, router).await?;

// Or use HTTP client
let client: HttpClient<dyn UserService> = HttpClient::new("http://localhost:8080");
client.get_user("123".into()).await?;
```

## License

MIT
