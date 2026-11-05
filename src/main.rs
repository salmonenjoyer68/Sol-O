use std::io::IsTerminal;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    if let Err(error) = sol_o::cli::run().await {
        eprintln!("sol-o: {error}");
        std::process::exit(1);
    }
}
