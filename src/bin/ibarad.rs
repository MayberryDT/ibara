//! `ibarad`: the long-lived ibara daemon. See `src/server/`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(u8::try_from(ibara::server::ibarad_main(args)).unwrap_or(1))
}
