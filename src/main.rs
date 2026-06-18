mod approval;
mod cli;
mod config;
mod event;
mod mcp;
mod mentions;
mod model;
mod provider;
mod runtime;
mod sandbox;
mod server;
mod tools;
mod tui;
mod verification;

fn main() {
    std::process::exit(cli::run());
}
