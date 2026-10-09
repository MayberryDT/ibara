//! `ibarad`: the long-lived ibara daemon. See `src/server/`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a|a=="native-field") { return ExitCode::from(ibara::desktop::native_field::main(&args[1..])); }
    ExitCode::from(u8::try_from(ibara::server::ibarad_main(args)).unwrap_or(1))
}
