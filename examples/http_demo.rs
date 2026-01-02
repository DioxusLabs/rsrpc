//! HTTP REST example for rsrpc
//!
//! Demonstrates using HTTP REST annotations alongside RPC.
//! Methods annotated with #[get], #[post], etc. are accessible via HTTP.
//!
//! Run the server: cargo run --example http_demo --features http -- server
//! Run the HTTP client: cargo run --example http_demo --features http -- http-client
//! Run the RPC client: cargo run --example http_demo --features http -- rpc-client

use anyhow::Result;
use rsrpc::{async_trait, Client, HttpClient};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// =============================================================================
// SERVICE DEFINITION WITH HTTP ANNOTATIONS
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    pub email: String,
}

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

    /// Internal RPC-only method (no HTTP annotation)
    async fn internal_sync(&self) -> Result<()>;
}

// =============================================================================
// SERVER IMPLEMENTATION
// =============================================================================

pub struct UserServiceImpl {
    users: tokio::sync::RwLock<Vec<User>>,
}

impl UserServiceImpl {
    pub fn new() -> Self {
        Self {
            users: tokio::sync::RwLock::new(vec![
                User {
                    id: "1".into(),
                    name: "Alice".into(),
                    email: "alice@example.com".into(),
                },
                User {
                    id: "2".into(),
                    name: "Bob".into(),
                    email: "bob@example.com".into(),
                },
            ]),
        }
    }
}

#[async_trait]
impl UserService for UserServiceImpl {
    async fn health(&self) -> Result<String> {
        Ok("OK".to_string())
    }

    async fn get_user(&self, id: String) -> Result<User> {
        let users = self.users.read().await;
        users
            .iter()
            .find(|u| u.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("User not found: {}", id))
    }

    async fn list_users(&self, limit: u32) -> Result<Vec<User>> {
        let users = self.users.read().await;
        Ok(users.iter().take(limit as usize).cloned().collect())
    }

    async fn create_user(&self, req: CreateUserRequest) -> Result<User> {
        let mut users = self.users.write().await;
        let id = (users.len() + 1).to_string();
        let user = User {
            id,
            name: req.name,
            email: req.email,
        };
        users.push(user.clone());
        Ok(user)
    }

    async fn delete_user(&self, id: String) -> Result<bool> {
        let mut users = self.users.write().await;
        let len_before = users.len();
        users.retain(|u| u.id != id);
        Ok(users.len() < len_before)
    }

    async fn internal_sync(&self) -> Result<()> {
        println!("Internal sync called (RPC only)");
        Ok(())
    }
}

// =============================================================================
// MAIN
// =============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("server");

    let rpc_port = std::env::var("RPC_PORT").unwrap_or_else(|_| "9000".into());
    let http_port = std::env::var("HTTP_PORT").unwrap_or_else(|_| "8080".into());

    match mode {
        "server" => {
            let service = Arc::new(UserServiceImpl::new());

            // Start RPC server
            let _rpc_service = service.clone();
            let rpc_addr = format!("127.0.0.1:{}", rpc_port);
            println!("Starting RPC server on {}...", rpc_addr);
            let rpc_server = <dyn UserService>::serve(UserServiceImpl::new());
            tokio::spawn(async move {
                if let Err(e) = rpc_server.listen(&rpc_addr).await {
                    eprintln!("RPC server error: {}", e);
                }
            });

            // Start HTTP server
            let http_addr = format!("127.0.0.1:{}", http_port);
            println!("Starting HTTP server on {}...", http_addr);
            let router = <dyn UserService>::http_routes(service);
            let listener = tokio::net::TcpListener::bind(&http_addr).await?;
            axum::serve(listener, router).await?;
        }

        "http-client" => {
            println!("Connecting via HTTP...");
            let client: HttpClient<dyn UserService> =
                HttpClient::new(&format!("http://127.0.0.1:{}", http_port));

            // Test health endpoint
            let health = client.health().await?;
            println!("Health: {}", health);

            // List users
            let users = client.list_users(10).await?;
            println!("Users: {:?}", users);

            // Get specific user
            let user = client.get_user("1".into()).await?;
            println!("User 1: {:?}", user);

            // Create new user
            let new_user = client
                .create_user(CreateUserRequest {
                    name: "Charlie".into(),
                    email: "charlie@example.com".into(),
                })
                .await?;
            println!("Created user: {:?}", new_user);

            // Delete user
            let deleted = client.delete_user(new_user.id).await?;
            println!("Deleted: {}", deleted);
        }

        "rpc-client" => {
            println!("Connecting via RPC...");
            let client: Client<dyn UserService> =
                Client::connect(&format!("127.0.0.1:{}", rpc_port)).await?;

            // All the same methods work via RPC
            let health = client.health().await?;
            println!("Health: {}", health);

            let users = client.list_users(10).await?;
            println!("Users: {:?}", users);

            // Plus RPC-only methods
            client.internal_sync().await?;
            println!("Internal sync completed");
        }

        _ => {
            println!("Usage: cargo run --example http_demo --features http -- [server|http-client|rpc-client]");
        }
    }

    Ok(())
}
