//! `ibara`: short-lived commands. One arm per subcommand; each returns the exit code.

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut argv = std::env::args_os().skip(1);
    let sub = argv.next().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let args: Vec<OsString> = argv.collect();
    match sub.as_str() {
        // Operator side (src/operator/).
        "client" | "operator" | "control" | "mcp" | "prompt" => ibara::operator::run_cli(&sub, args),
        // Target side (src/entry/).
        "agent-entry" => code(ibara::entry::agent_entry(args)),
        "access-system" => code(ibara::access_system::main(args)),
        "power-system" => code(ibara::power_system::main(args)),
        "admin" => code(ibara::entry::admin(args)),
        "chrome-host" => code(ibara::entry::chrome_host(args)),
        "browser-setup" => code(ibara::entry::browser_setup::main(args)),
        "backup-journals" => code(ibara::entry::backup_journals(args)),
        "join" => code(ibara::entry::join::main(args)),
        "away" => code(ibara::console::fleet::away_main(args)),
        "video" => code(ibara::console::video::main(args)),
        // Installing, updating and removing ibara (src/install/).
        "setup" => ibara::install::setup_main(args),
        "uninstall" => ibara::install::uninstall_main(args),
        "update" => ibara::install::update_main(args),
        "rollback" => ibara::install::rollback_main(args),
        "system" => ibara::install::system_main(args),
        "unattended-boot" => ibara::install::unattended_boot_main(args),
        // End-to-end runs of the agent contract (src/harness/).
        "harness" => code(ibara::harness::main(args)),
        "--help" | "-h" | "help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        "--version" | "-V" => {
            println!("ibara {}", ibara::version());
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(64)
        }
    }
}

/// The commands a person runs. The rest are started by ibara itself and stay unlisted.
const USAGE: &str = "Usage: ibara COMMAND …

Commands:
  setup             Set ibara up for you on this computer (run after installing or updating)
  update            Install the latest ibara release
  rollback          Go back to the release installed before this one
  uninstall         Remove ibara (--delete-data also removes its data)
  prompt            Print the prompt that connects an agent
  mcp               The agent connection an agent's settings start
  client            Send and get files, and name computers
  join              Answer a request to add this computer
  unattended-boot   Let this computer start ibara before anyone signs in
  --version         Print ibara's version

To connect an agent, copy the prompt from Connect an Agent in the ibara console and paste it to your agent;
it connects itself. ibara prompt prints the same prompt.
";

fn code(status: i32) -> ExitCode {
    ExitCode::from(u8::try_from(status).unwrap_or(1))
}
