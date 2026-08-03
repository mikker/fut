fn main() -> std::process::ExitCode {
    fut::cli::complete();

    tokio::runtime::Runtime::new()
        .expect("create Tokio runtime")
        .block_on(fut::cli::run())
}
