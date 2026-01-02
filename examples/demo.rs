//! Example usage of rrpc
//!
//! This demonstrates the ergonomic API where:
//! - Users define a trait with `#[rrpc::service]`
//! - `Client<dyn Trait>` automatically implements `Trait`
//! - Server is created with `serve_<trait_name>(impl)`
//!
//! Run the server: cargo run --example demo -- server
//! Run the client: cargo run --example demo -- client

use anyhow::Result;
use rrpc::{async_trait, Client};
use serde::{Deserialize, Serialize};

// =============================================================================
// DEFINE THE SERVICE TRAIT - THIS IS YOUR API
// =============================================================================

#[rrpc::service]
pub trait VmManager: Send + Sync + 'static {
    async fn start_vm(&self, vm_id: String) -> Result<bool>;
    async fn stop_vm(&self, vm_id: String) -> Result<bool>;
    async fn get_vm_status(&self, vm_id: String) -> Result<VmStatus>;
    async fn list_vms(&self) -> Result<Vec<String>>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmStatus {
    pub running: bool,
    pub cpu_usage: f32,
    pub memory_mb: u64,
}

// =============================================================================
// SERVER IMPLEMENTATION - JUST IMPLEMENT THE TRAIT NORMALLY
// =============================================================================

pub struct MacosVmManager {
    // ... fields ...
}

#[async_trait]
impl VmManager for MacosVmManager {
    async fn start_vm(&self, vm_id: String) -> Result<bool> {
        println!("Starting VM: {}", vm_id);
        Ok(true)
    }

    async fn stop_vm(&self, vm_id: String) -> Result<bool> {
        println!("Stopping VM: {}", vm_id);
        Ok(true)
    }

    async fn get_vm_status(&self, _vm_id: String) -> Result<VmStatus> {
        Ok(VmStatus {
            running: true,
            cpu_usage: 45.2,
            memory_mb: 4096,
        })
    }

    async fn list_vms(&self) -> Result<Vec<String>> {
        Ok(vec!["vm-1".into(), "vm-2".into(), "vm-3".into()])
    }
}

// =============================================================================
// CLIENT USAGE - CALL METHODS DIRECTLY ON THE CLIENT
// =============================================================================

async fn run_client() -> Result<()> {
    println!("Connecting to server...");

    // Client<dyn VmManager> implements VmManager!
    let client: Client<dyn VmManager> = Client::connect("127.0.0.1:8080").await?;

    // Call methods directly - just like a local implementation
    println!("Starting VM...");
    client.start_vm("vm-123".into()).await?;

    println!("Getting VM status...");
    let status = client.get_vm_status("vm-123".into()).await?;
    println!("VM status: {:?}", status);

    println!("Listing VMs...");
    let vms = client.list_vms().await?;
    println!("All VMs: {:?}", vms);

    println!("Stopping VM...");
    client.stop_vm("vm-123".into()).await?;

    Ok(())
}

// =============================================================================
// POLYMORPHISM - WORKS WITH BOTH LOCAL AND REMOTE IMPLS
// =============================================================================

/// This function accepts both local implementations AND remote clients!
async fn do_vm_work(manager: &impl VmManager) -> Result<()> {
    let vms = manager.list_vms().await?;
    for vm_id in vms {
        let status = manager.get_vm_status(vm_id).await?;
        println!("Status: {:?}", status);
    }
    Ok(())
}

// =============================================================================
// MAIN
// =============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("server");

    match mode {
        "server" => {
            let manager = MacosVmManager {};

            // Demonstrate polymorphism with local impl
            println!("Running with local implementation:");
            do_vm_work(&manager).await?;

            // Create and run server
            println!("\nStarting server on 127.0.0.1:8080...");
            let server = serve_vm_manager(manager);
            server.listen("127.0.0.1:8080").await?;
        }
        "client" => {
            run_client().await?;
        }
        _ => {
            println!("Usage: cargo run --example demo -- [server|client]");
        }
    }

    Ok(())
}
