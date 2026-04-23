use slide_leaner::{Args, run};

fn main() {
    let start_time = std::time::Instant::now();

    let args = Args::build(std::env::args()).unwrap_or_else(|err| {
        eprintln!("Problem parsing arguments: {}", err);
        std::process::exit(1);
    });
    run(args);

    let elapsed = start_time.elapsed();
    println!("Total execution time: {:.2?}", elapsed);
}
