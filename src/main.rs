fn main() {
    if let Err(error) = tp7::run() {
        eprintln!("error: {}", tp7::terminal_safe(&error.to_string()));
        std::process::exit(error.exit_code());
    }
}
