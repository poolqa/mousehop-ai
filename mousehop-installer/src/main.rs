#[cfg(windows)]
mod native;

fn main() {
    #[cfg(windows)]
    {
        if let Err(error) = native::run() {
            eprintln!("Mousehop installer: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("mousehop-installer runs only on Windows");
        std::process::exit(1);
    }
}
