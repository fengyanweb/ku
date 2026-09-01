// Expanded import graphs are checked by a few intentionally recursive compiler
// passes. Windows PE executables normally reserve only 1 MiB for their main
// thread, while Unix main-thread stacks are commonly larger. Run exactly one
// CLI invocation on a bounded, platform-independent worker stack instead of
// making correctness depend on linker flags or user environment variables.
const CLI_WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

fn run_on_cli_worker(args: Vec<String>) -> std::thread::Result<Result<(), ku::error::KuError>> {
    match std::thread::Builder::new()
        .name("ku-cli".to_string())
        .stack_size(CLI_WORKER_STACK_BYTES)
        .spawn(move || ku::cli::run_cli(args))
    {
        Ok(worker) => worker.join(),
        Err(error) => Ok(Err(ku::error::KuError::runtime(
            format!("failed to start CLI worker: {error}"),
            ku::span::Span::default(),
        ))),
    }
}

fn main() {
    let result = match run_on_cli_worker(std::env::args().collect()) {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    if let Err(err) = result {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
