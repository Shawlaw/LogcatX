//! Update helper shipped beside LogcatX.exe. The main application hands over
//! an apply plan and exits; this process then replaces the release files and
//! restarts the app, keeping rollback copies until the new build acknowledges
//! a successful start.

fn main() {
    if let Err(error) = desktop_updater::run_helper_from_args() {
        eprintln!("LogcatX updater: {error}");
        std::process::exit(1);
    }
}
