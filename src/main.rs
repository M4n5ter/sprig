#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("Sprig 0.1 supports Linux x86-64 and AArch64 only");

mod cli;
mod copy_tree;
mod store;

fn main() -> std::process::ExitCode {
    match cli::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sprig: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
