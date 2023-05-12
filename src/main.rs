mod clipboard;

use std::io;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct AppArgs {
    #[command(subcommand)]
    command: AppCommands,
}

#[derive(Subcommand, Debug)]
enum AppCommands {
    #[command(subcommand)]
    Clipboard(clipboard::ClipboardCommands),
}

fn execute(args: AppArgs) -> io::Result<()> {
    match args.command {
        AppCommands::Clipboard(clipboard_args) => clipboard::execute(clipboard_args),
    }
}

fn main() -> io::Result<()> {
    execute(AppArgs::parse())
}
