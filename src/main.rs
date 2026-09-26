fn main() {
    if let Err(error) = meowland::start() {
        eprintln!("meowland: {error}");
        std::process::exit(1);
    }
}
