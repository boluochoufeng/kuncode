use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "kuncode", version, about = "KunCode agent harness runtime")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
