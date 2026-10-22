#![deny(unsafe_op_in_unsafe_fn)]

mod cli;

fn main() {
    std::process::exit(cli::run());
}
