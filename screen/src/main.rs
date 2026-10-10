mod capture;
#[cfg(feature="qualification")]
mod transport_probe;
mod gnome_capture;
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
fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create screen runtime");
    let result = runtime.block_on(run());
    // Sender cleanup has settled input and joined capture before returning.
    // Tokio's blocking stdin read cannot be cancelled while its parent pipe
    // stays open; it must not keep the finished process alive.
    runtime.shutdown_timeout(std::time::Duration::from_millis(250));
    if let Err(e) = result {
        eprintln!("ibara-screen: {e:#}");
        std::process::exit(1);
    }
}
async fn run() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let home = std::path::PathBuf::from(std::env::var("HOME")?);
    match args.first().map(String::as_str) {
        #[cfg(feature = "qualification")]
        Some("capture-boundary-proof") => {
            let root=args.get(1).ok_or_else(||anyhow::anyhow!("Expected boundary fixture root"))?.clone();
            tokio::task::spawn_blocking(move ||gnome_capture::boundary_proof(std::path::Path::new(&root))).await?
        },
        #[cfg(feature = "qualification")]
        Some("capture-revocation-proof") => {
            let path=args.get(1).ok_or_else(||anyhow::anyhow!("Expected revocation receipt output"))?.clone();
            tokio::task::spawn_blocking(move ||gnome_capture::revocation_proof(std::path::Path::new(&path))).await?
        },
        #[cfg(feature = "qualification")]
        Some("capture-stream-proof") => {
            let path=args.get(1).ok_or_else(||anyhow::anyhow!("Expected synthetic stream output"))?;
            gnome_capture::stream_proof(std::path::Path::new(path)).await
        },
        #[cfg(feature = "qualification")]
        Some("capture-proof") => {
            let path=args.get(1).ok_or_else(||anyhow::anyhow!("Expected synthetic fixture PPM output"))?.clone();
            tokio::task::spawn_blocking(move ||gnome_capture::proof(std::path::Path::new(&path))).await?
        },
        #[cfg(feature = "qualification")]
        Some("transport-proof") => {
            let dir=args.get(1).ok_or_else(||anyhow::anyhow!("Expected private receiver directory"))?;
            let output=args.get(2).ok_or_else(||anyhow::anyhow!("Expected H.264 output"))?;
            transport_probe::run(std::path::Path::new(dir),std::path::Path::new(output)).await
        },
        #[cfg(feature = "qualification")]
        Some("send-proof") => {
            let dir=args.get(1).ok_or_else(||anyhow::anyhow!("Expected private state directory"))?;
            sender::proof_run(std::path::Path::new(dir)).await
        },
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
