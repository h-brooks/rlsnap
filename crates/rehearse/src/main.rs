#[tokio::main]
async fn main() {
    let exit_code = rehearse::run(std::env::args()).await;
    std::process::exit(exit_code);
}
