fn main() {
    if let Err(error) = vitals_agent::run() {
        eprintln!("vitals-agent: {error}");
        std::process::exit(1);
    }
}
