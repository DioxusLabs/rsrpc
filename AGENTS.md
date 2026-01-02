# RPC Library Development

## Project Goal

Build an ergonomic, function-forward RPC library for Rust-to-Rust communication. Primary use case is controller-worker architecture on a server rack.

## Design Principles

1. **The trait IS the API** - User defines a trait, both client and server use that exact trait
2. **Client implements the trait** - `Client<dyn MyTrait>` implements `MyTrait`, so `client.method()` just works
3. **No visible message types** - Request/response structs are internal, not exposed in public API
4. **Library owns generic types** - Library provides `Server<T>` and `Client<S>`, user defines their service trait
5. **No enum dispatch** - Each method gets its own request struct + method ID, avoiding deserialization ambiguity
6. **Polymorphism works** - `fn do_work(w: &impl MyTrait)` accepts both local impl and remote client

## Why Not Existing Solutions

- **tarpc**: Client codegen not discoverable, trait definition doesn't match client impl, mandatory Context parameter
- **gRPC/tonic**: Overkill for pure Rust, cross-language schema not needed
- **Raw WebSocket**: Too low-level, have to build dispatch/correlation yourself

## Current Implementation

Working implementation with:
- `Server<T>` - wraps service impl, handles TCP connections
- `Client<T>` - connects to remote, implements the service trait
- `#[rrpc::service]` - proc macro that generates client/server glue
- Wire format: `[method_id: u16][request_id: u64][payload_len: u32][payload]`
- Uses `postcard` for serialization

### What the macro generates:
- Per-method request structs (private)
- Method ID constants (private)
- Dispatch function that routes by method ID
- `impl MyTrait for Client<dyn MyTrait>`
- `<dyn MyTrait>::serve(impl) -> Server<dyn MyTrait>`

## Next Steps

1. ~~**Write the proc macro**~~ - Done: `#[rrpc::service]` generates glue code from trait definition
2. ~~**Error handling**~~ - Done: errors converted to strings for wire transport
3. **Connection management** - Reconnection, health checks, timeouts
4. **Streaming support** - Methods returning `impl Stream<Item = T>`
5. **Bidirectional** - Server-initiated calls to workers
6. **Testing** - Add integration tests, edge case coverage

## Architecture Decisions Made

| Decision | Rationale |
|----------|-----------|
| TCP not HTTP | Server-to-server, no browser involved, simpler |
| postcard serialization | Fast, compact, serde-compatible (can swap for rkyv) |
| Method IDs not names | Smaller wire format, faster dispatch |
| No enum for requests | Deserializer knows exactly what type to expect |
| `dyn Trait` in Client type param | Enables `impl Trait for Client<dyn Trait>` pattern |

## Code Style

- Minimal dependencies
- No unnecessary abstractions
- Generated code should be inspectable and unsurprising
- Prefer compile-time errors over runtime errors

## Files

- `packages/lib/src/lib.rs` - Core library (Client, Server, wire protocol)
- `packages/macro/src/lib.rs` - Proc macro implementation
- `examples/demo.rs` - Working example with VmManager trait

## Example of Target API

```rust
// User writes this
#[rpc::service]
pub trait Worker: Send + Sync + 'static {
    async fn run_task(&self, task: Task) -> Result<Output, Error>;
    async fn status(&self) -> WorkerStatus;
}

// User implements the trait directly
impl Worker for MyWorker {
    async fn run_task(&self, task: Task) -> Result<Output, Error> {
        // ...
    }
    async fn status(&self) -> WorkerStatus {
        // ...
    }
}

// Server
let server = <dyn Worker>::serve(my_worker);
server.listen("0.0.0.0:9000").await?;

// Client - same method signatures!
let client: Client<dyn Worker> = Client::connect("10.0.0.5:9000").await?;
client.run_task(task).await?;
client.status().await;
```
