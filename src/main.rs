use std::io;

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    harper_plugin::run(stdin.lock(), stdout.lock())
}
