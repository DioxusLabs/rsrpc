# rsrpc-macro

Procedural macros for [rsrpc](https://crates.io/crates/rsrpc) - Ergonomic Rust-to-Rust RPC.

This crate provides the `#[rsrpc::service]` attribute macro that transforms a trait definition into a full RPC service with client and server implementations.

## Usage

This crate is not meant to be used directly. Instead, use the main `rsrpc` crate which re-exports the macro:

```rust
#[rsrpc::service]
pub trait MyService: Send + Sync + 'static {
    async fn hello(&self, name: String) -> Result<String>;
}
```

See the [rsrpc documentation](https://docs.rs/rsrpc) for complete usage instructions.

## License

MIT
