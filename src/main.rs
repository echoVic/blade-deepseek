use std::env;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "-V" || arg == "--version") {
        println!("orca {VERSION}");
        return;
    }

    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return;
    }

    if args.is_empty() {
        println!("Orca");
        println!("A DeepSeek-native coding agent runtime by Blade.");
        println!();
        println!("Run `orca --help` for usage.");
        return;
    }

    println!("Orca runtime is not implemented yet.");
    println!("Received: {}", args.join(" "));
}

fn print_help() {
    println!("Orca");
    println!("A DeepSeek-native coding agent runtime by Blade.");
    println!();
    println!("Usage:");
    println!("  orca [prompt]");
    println!("  orca exec [options] <prompt>");
    println!();
    println!("Options:");
    println!("  -h, --help       Show help");
    println!("  -V, --version    Show version");
}
