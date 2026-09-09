use clap::Parser;
use thinslide::{Args, run, ensure_jpegtables_tag_registered};

fn main() {
    ensure_jpegtables_tag_registered();
    let start_time = std::time::Instant::now();

    let args = Args::parse();
    run(args);

    let elapsed = start_time.elapsed();
    println!("Total execution time: {:.2?}", elapsed);
}
