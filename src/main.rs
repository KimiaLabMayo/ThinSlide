use dcm2tiff::{Args, run};

fn main() {

    let args = Args::build(std::env::args()).unwrap_or_else(|err| {
        eprintln!("Problem parsing arguments: {}", err);
        std::process::exit(1);
    });
    run(args);

}