use anyhow::Result;

use crate::rrrpc::{Client, ClientDriver};

#[async_trait::async_trait(?Send)]
pub trait VmManager {
    async fn start_vm(&self, vm_id: &str) -> Result<bool>;
    async fn stop_vm(&self, vm_id: &str) -> Result<bool>;
    async fn get_vm_status(&self, vm_id: &str) -> Result<String>;
}

#[async_trait::async_trait(?Send)]
impl VmManager for Client<dyn VmManager> {
    async fn start_vm(&self, vm_id: &str) -> Result<bool> {
        todo!()
    }
    async fn stop_vm(&self, vm_id: &str) -> Result<bool> {
        todo!()
    }
    async fn get_vm_status(&self, vm_id: &str) -> Result<String> {
        todo!()
    }
}

async fn main() -> Result<()> {
    let con = rrrpc::Client::<dyn VmManager>::tcp("127.0.0.1:8080").await?;

    con.start_vm("vm123").await?;
    con.stop_vm("123").await?;
    con.get_vm_status("vm123").await?;

    Ok(())
}

mod server {
    use super::VmManager;

    pub struct VmManagerMacosWorker {
        // Add any necessary fields here
    }

    #[async_trait::async_trait(?Send)]
    impl VmManager for VmManagerMacosWorker {
        async fn start_vm(&self, vm_id: &str) -> anyhow::Result<bool> {
            todo!()
        }
        async fn stop_vm(&self, vm_id: &str) -> anyhow::Result<bool> {
            todo!()
        }
        async fn get_vm_status(&self, vm_id: &str) -> anyhow::Result<String> {
            todo!()
        }
    }

    pub async fn run_server() -> anyhow::Result<()> {
        let worker = VmManagerMacosWorker {};

        Ok(())
    }
}

mod rrrpc {
    pub struct Client<T: ?Sized> {
        _marker: std::marker::PhantomData<T>,
    }

    impl<T: ?Sized> Client<T> {
        pub async fn tcp(_addr: &str) -> anyhow::Result<Self> {
            todo!()
        }
    }

    pub trait ClientDriver {}

    struct ImplMarker;
}
