mod approval;
mod cli;
mod config;
mod event;
mod mcp;
mod provider;
mod runtime;
mod sandbox;
mod tools;
mod tui;
mod verification;

fn main() {
    std::process::exit(cli::run());
}
