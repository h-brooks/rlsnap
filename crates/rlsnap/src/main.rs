#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir().expect("failed to read current directory");
    match rlsnap::run(&args, &cwd).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("rlsnap: error: {e:#}");
            std::process::exit(2);
        }
    }
}
