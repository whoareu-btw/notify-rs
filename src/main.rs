use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    if let Some(code) = notify_rs::cli::run(cmd, &args) {
        return code;
    }
    if let Some(code) = notify_rs::ctl::run(cmd, &args) {
        return code;
    }
    match cmd {
        "preview" => notify_rs::ui::preview::run(&args),
        "fonts" => notify_rs::ui::preview::fonts(),
        "help" | "-h" | "--help" => {
            print!("{}{}{}    notify-rs help | version\n", notify_rs::cli::HELP, notify_rs::ctl::HELP, notify_rs::ui::preview::HELP);
            ExitCode::SUCCESS
        }
        "version" | "-V" | "--version" => {
            println!("notify-rs {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command: {other}\n\n{}{}{}", notify_rs::cli::HELP, notify_rs::ctl::HELP, notify_rs::ui::preview::HELP);
            ExitCode::from(2)
        }
    }
}
