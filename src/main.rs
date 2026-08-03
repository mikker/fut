#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fut::cli::run().await
}
