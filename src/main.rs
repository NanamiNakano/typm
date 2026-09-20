use clap::Parser;

fn main() {
    match typm::run(typm::cli::Args::parse()) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {}", snafu::Report::from_error(error));
            std::process::exit(1);
        }
    }
}
