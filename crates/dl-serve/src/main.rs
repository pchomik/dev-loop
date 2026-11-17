mod agent;
mod config;
mod db;
mod integrations;
mod log;
mod ipc;
mod runner;

fn main() {
    if let Err(e) = ipc::serve(&config::Paths::from_env()) {
        eprintln!("dl-serve: {e}");
        std::process::exit(1);
    }
}
