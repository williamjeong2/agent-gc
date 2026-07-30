use std::process::ExitCode;

fn main() -> ExitCode {
    match agent_gc::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
