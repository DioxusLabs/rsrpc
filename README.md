# rrpc

Ergonomic Rust-to-Rust RPC where the trait is the API.

## Overview

rrpc generates RPC client and server code from a trait definition. The client implements the same trait as the server, so `client.method(args)` just works. No separate client types, no message enums, no schema files.

```rust
#[rrpc::service]
pub trait Worker: Send + Sync + 'static {
    async fn run_task(&self, task: Task) -> Result<Output>;
    async fn status(&self) -> Result<WorkerStatus>;
}
```

The macro generates:
- `impl Worker for Client<dyn Worker>` — call methods directly on the client
- `<dyn Worker>::serve(impl)` — wrap any implementation in a server

## Usage

Define your service trait:

```rust
use anyhow::Result;
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
pub struct VmStatus {
    pub running: bool,
    pub memory_mb: u64,
}

#[rrpc::service]
pub trait VmManager: Send + Sync + 'static {
    async fn start_vm(&self, vm_id: String) -> Result<bool>;
    async fn stop_vm(&self, vm_id: String) -> Result<bool>;
    async fn get_status(&self, vm_id: String) -> Result<VmStatus>;
}
```

Implement and run the server:

```rust
use rrpc::async_trait;

struct MyVmManager { /* ... */ }

#[async_trait]
impl VmManager for MyVmManager {
    async fn start_vm(&self, vm_id: String) -> Result<bool> {
        // implementation
    }
    // ...
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = <dyn VmManager>::serve(MyVmManager::new());
    server.listen("0.0.0.0:9000").await
}
```

Connect from a client:

```rust
use rrpc::Client;

#[tokio::main]
async fn main() -> Result<()> {
    let client: Client<dyn VmManager> = Client::connect("10.0.0.5:9000").await?;

    client.start_vm("vm-1".into()).await?;
    let status = client.get_status("vm-1".into()).await?;

    Ok(())
}
```

## Polymorphism

Because `Client<dyn VmManager>` implements `VmManager`, generic code works with both local and remote implementations:

```rust
async fn check_all_vms(manager: &impl VmManager) -> Result<()> {
    for vm in manager.list_vms().await? {
        let status = manager.get_status(vm).await?;
        println!("{}: {:?}", vm, status);
    }
    Ok(())
}

// Works with local implementation
check_all_vms(&my_local_manager).await?;

// Works with remote client
check_all_vms(&client).await?;
```

## Wire Protocol

Messages use a simple binary format:

| Field | Size | Description |
|-------|------|-------------|
| method_id | 2 bytes | Method identifier (u16 LE) |
| request_id | 8 bytes | Request correlation ID (u64 LE) |
| payload_len | 4 bytes | Payload length (u32 LE) |
| payload | variable | Serialized request/response |

Payloads are serialized with [postcard](https://docs.rs/postcard). Method return types are wrapped as `Result<T, String>` on the wire, allowing any error type on the server side.

## Requirements

- All method arguments and return types must implement `Serialize` and `Deserialize`
- Traits must have `Send + Sync + 'static` bounds
- Methods must be `async fn(&self, ...)`

## Why not X?

**tarpc**: Client codegen isn't discoverable from the trait, requires a mandatory `Context` parameter, trait definition doesn't match the client interface.

**tonic/gRPC**: Requires protobuf schemas, overkill for pure Rust services where cross-language support isn't needed.

**Raw TCP/WebSocket**: Too low-level. You end up building dispatch tables and request correlation yourself.

## License

MIT
