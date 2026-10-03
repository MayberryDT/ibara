mod capture;
mod codec;
mod nvenc;
mod nvenc_egl;
mod identity;
mod input;
mod keyboard_toggle;
mod render;
mod sender;
mod software;
mod viewer;
mod vpp;
fn now_us() -> u64 {
    ibara_screen::now_us()
}
fn now_ms() -> u64 {
    ibara_screen::now_ms()
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("ibara-screen: {e:#}");
        std::process::exit(1);
    }
}
async fn run() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let home = std::path::PathBuf::from(std::env::var("HOME")?);
    match args.first().map(String::as_str) {
        Some("send") => {
            let dir = args
                .windows(2)
                .find(|a| a[0] == "--state-dir")
                .map(|a| std::path::PathBuf::from(&a[1]))
                .unwrap_or(home.join(".local/state/Ibara/screen"));
            sender::run(&dir).await
        }
        Some("view") if args.iter().any(|a| a == "--toggle-keys") => {
            keyboard_toggle::toggle_focused()
        }
        Some("view") if args.iter().any(|a| a == "--identity") => {
            let id = identity::Identity::load(&home.join(".config/Ibara/screen"))?;
            println!("{}", identity::hash(&id.cert));
            Ok(())
        }
        Some("view") => viewer::run(&home.join(".config/Ibara/screen")).await,
        _ => anyhow::bail!("Usage: ibara-screen send [--state-dir DIR] | view [--identity]"),
    }
}
/// Keep stream reads alive across select branches: length-prefixed reads cannot
/// be cancelled safely after consuming part of their header or payload.
fn control_reader(
    mut stream: quinn::RecvStream,
) -> (
    tokio::sync::mpsc::Receiver<anyhow::Result<serde_json::Value>>,
    AbortTask<()>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let task = tokio::spawn(async move {
        loop {
            let result = ibara_screen::wire::read_json(&mut stream).await;
            let failed = result.is_err();
            if tx.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    (rx, AbortTask(task))
}
struct AbortTask<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
