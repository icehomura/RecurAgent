use clap::CommandFactory;
fn main() {
    let mut cmd = ra::cli::Cli::command();
    cmd.build();
    for a in cmd.get_arguments() {
        if let Some(long) = a.get_long() {
            println!("{long}");
        }
    }
}
