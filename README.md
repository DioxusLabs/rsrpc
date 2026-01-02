# rsrpc

Ergonomic Rust-to-Rust RPC, perfect for web services and web development.

```rust
// 1. Define your interface with a trait.
#[rsrpc::service]
pub trait Worker: Send + Sync + 'static {
    async fn run_task(&self, task: Task) -> Result<Output>;
    async fn status(&self) -> Result<WorkerStatus>;
}

// 2. Implement the trait for your different backends
pub struct MacosWorker;
#[async_trait]
impl Worker for MacosWorker {
    async fn run_task(&self, task: Task) -> Result<Output> {
        // ...
    }
    async fn status(&self) -> Result<Output> {
        // ...
    }
}

// 3. Start your server
<dyn Worker>::serve(MacosWorker).listen("0.0.0.0:9000").await

// 4. Call RPC methods from the rsrpc `Client<T>`
let client = Client::<dyn Worker>::connect("10.0.0.5:9000").await?;
let result = client.status()?;
let output = client.run_task(Task::foo())?;
```

## Stream Support

The `Encoding` trait lets you define custom RPC types, useful for foreign object implemenations and streams.

The `RpcStream` type wraps `impl Stream`, making it easy to implement status updates and progress indicators.

```rust
#[rsrpc::service]
pub trait LogService: Send + Sync + 'static {
    // Use `RpcStream` to return or send a stream of LogEntry items over a single call (no polling!)
    async fn stream_logs(&self, filter: LogFilter) -> Result<RpcStream<LogEntry>>;
}
```

## HTTP Client Support

Use `RpcStream` over REST with the optional `#[get]`/`#[post]` endpoint add-on. Perfect for web development!

Todo: OpenAPI support and header extraction.

```rust
/// A user management service with both RPC and HTTP endpoints.
#[rsrpc::service]
pub trait UserService: Send + Sync + 'static {
    /// Health check - available via HTTP GET /health
    #[get("/health")]
    async fn health(&self) -> Result<String>;

    /// Get user by ID - available via HTTP GET /users/:id
    #[get("/users/{id}")]
    async fn get_user(&self, id: String) -> Result<User>;

    /// List all users with optional limit - HTTP GET /users?limit=N
    #[get("/users/?limit")]
    async fn list_users(&self, limit: u32) -> Result<Vec<User>>;

    /// Create a new user - HTTP POST /users with JSON body
    #[post("/users")]
    async fn create_user(&self, req: CreateUserRequest) -> Result<User>;

    /// Delete a user - HTTP DELETE /users/:id
    #[delete("/users/{id}")]
    async fn delete_user(&self, id: String) -> Result<bool>;
}
```

## Multiple Backend Implementations

The `Client<T>` type implements *your* trait, making it easy to mock different backends and unify protocols across heterogeneous fleets.

```rust
struct MacosServerImpl;
impl Worker for MacosImpl {
    // ..
}

struct WindowsServerImpl;
impl Worker for WindowsServerImpl {
    // ..
}
```

## Easy Mocking

No remote? No problem. Spawn workers locally or mock them.

```rust
struct LocalWorker;
impl Worker for LocalWorker {
    // ..
}
```

## Why not X?

This crate was designed to be a better *pure Rust* RPC. Eventually, we plan to add cross-language binding and code-generation support. Other RPC solutions are popular, but their code generation is not very discoverable or ergonomic.

With rsrpc, what you see is what you get. No extra types, traits, mods, etc. The only magic is that your trait is automatically implemented for the rspsc `Client<T>` type.

**tarpc**: Client codegen isn't discoverable from the trait, requires a mandatory `Context` parameter, trait definition doesn't match the client interface.

**tonic/gRPC**: Requires protobuf schemas, overkill for pure Rust services where cross-language support isn't needed.

**Raw TCP/WebSocket**: Too low-level. You end up building dispatch tables and request correlation yourself.

## License

MIT
