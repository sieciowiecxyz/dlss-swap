use std::process::ExitCode;

mod app;

fn main() -> ExitCode {
    match app::execute(std::env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dlss-swap: {error}");
            ExitCode::FAILURE
        }
    }
}
