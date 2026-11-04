use clap::Parser;
fn main() {
    let args = mousehop_cli::CliArgs::parse();
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("CLI runtime")
        .block_on(mousehop_cli::run(args));
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
