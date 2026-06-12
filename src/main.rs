fn main() {
    if let Err(err) = ku::cli::run_cli(std::env::args().collect()) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
