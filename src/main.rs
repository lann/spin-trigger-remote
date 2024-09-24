use spin_runtime_factors::FactorsBuilder;
use spin_trigger::{anyhow, cli::FactorsTriggerCommand, Parser};
use spin_trigger_remote::RemoteTrigger;

type Command = FactorsTriggerCommand<RemoteTrigger, FactorsBuilder>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _guard = spin_telemetry::init("remote".into())?;
    Command::parse().run().await
}
