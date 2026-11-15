//! Stand-alone build of RingScan's headless CLI (`ringscan --cli ...`).
//!
//! ```text
//! ringscan-cli [--cli] <PATH|X:|all> [--json] [--method auto|mft|walk]
//!              [--depth N] [--top N] [--threads N] [--logical] [--all-links]
//! ```

fn main() {
    ringscan_win::disable_error_dialogs();
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(ringscan_engine::run_cli(&args));
}
