mod approval;
mod cli;
mod config;
mod event;
mod provider;
mod runtime;
mod tools;
mod verification;

fn main() {
    std::process::exit(cli::run());
}
