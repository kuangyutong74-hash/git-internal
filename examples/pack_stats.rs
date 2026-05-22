use git_internal::internal::pack::analyze_pack;
use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() != 2 {
        eprintln!("Usage: {} <pack-file>", args[0]);
        std::process::exit(1);
    }

    match analyze_pack(&args[1]) {
        Ok(stats) => {
            println!("Pack statistics: {:?}", stats);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
