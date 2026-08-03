#[tokio::main]
async fn main() -> std::process::ExitCode {
    fut::cli::run().await
}
