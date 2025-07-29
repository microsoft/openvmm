use crate::spec;
use async_trait::async_trait;

mod admin;
pub mod pci;

#[async_trait]
trait FaultInjector {
    async fn inject(&self, cmd: spec::Command) -> spec::Command;
}
