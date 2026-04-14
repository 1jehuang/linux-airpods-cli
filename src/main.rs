fn main() {
    if let Err(error) = linux_airpods_cli::run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
