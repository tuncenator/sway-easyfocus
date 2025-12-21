use std::fs::File;
use std::rc::Rc;

use clap::Parser;
use figment::Figment;
use figment::providers::{Format, Yaml};

use crate::options::Options;

mod cli;
mod options;
mod sway;
mod ui;
mod util;

fn obtain_lock() -> File {
    let mut path = std::env::temp_dir();
    path.push("sway-easyfocus-lockfile");

    let file = File::create(path).expect("failed to open the lockfile");
    file.try_lock()
        .expect("failed to lock the lockfile (is another instance running?)");

    file
}

fn read_options() -> Rc<Options> {
    let mut opts = Options::default();

    let base_dirs = xdg::BaseDirectories::with_prefix("sway-easyfocus");
    let config_path = base_dirs
        .place_config_file("config.yaml")
        .expect("failed to create config directory");

    if let Ok(args) = Figment::new()
        .merge(Yaml::file(&config_path))
        .extract::<cli::Args>()
    {
        opts.merge(&args);
    }

    let cli_args = cli::Args::parse();
    opts.merge(&cli_args);

    Rc::new(opts)
}

fn main() {
    let opts = read_options();

    let lockfile = obtain_lock();
    match swayipc::Connection::new() {
        Ok(conn) => ui::run_ui(conn, opts),
        Err(_) => eprintln!("Failed to connect to sway."),
    }
    lockfile.unlock().expect("failed to unlock the lockfile");
}
