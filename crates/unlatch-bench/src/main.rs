//! `unlatch-bench` — network lab, benchmarks and verification harness. See `bench/README.md`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match unlatch_bench::cli::main(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("unlatch-bench: {e:#}");
            2
        }
    };
    std::process::exit(code);
}
