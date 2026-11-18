//! `amui <command> [args]`: the engine's command line. Everything is in [`amui::cli`]; this
//! hands it the words of the command line and the configuration of the process, and ends with
//! the exit status it returns.

use std::process::ExitCode;
use std::sync::Arc;

use amui::config::Config;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os().skip(1).map(|word| word.to_string_lossy().into_owned()).collect();
    // `amui wrapper ARGS`: the lossless route's control helper (what `python3
    // tools/lossless/wrapperctl.py ARGS` was), which the engine starts for the sign-in and the
    // service. It takes the process over: its own umask, signals and streams.
    if args.first().is_some_and(|word| word == "wrapper") {
        let status = amui::wrapper::main("amui wrapper", &args[1..]);
        return ExitCode::from(status.rem_euclid(256) as u8);
    }
    let mut config = Config::from_process();
    // this program is the lossless route's control helper too (`amui wrapper`)
    if let Ok(program) = std::env::current_exe() {
        config.run_helper_as(program, &["wrapper"]);
    }
    let status = amui::cli::run(Arc::new(config), &args);
    // the low eight bits, as a shell sees any exit status
    ExitCode::from(status.rem_euclid(256) as u8)
}
