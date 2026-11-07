//! The `wsblast` command-line entry point. The command lives in the library
//! so the binary and the library share one engine.

/// Stack of the thread that runs the command: as much as Linux gives a
/// main thread. Windows gives the main thread 1 MiB, and in an unoptimized
/// build the parser clap derives for the command line registers every flag
/// in one function whose frame alone is about 650 KiB, so on the main
/// thread a debug build overflowed before it parsed anything. The memory
/// is reserved, not used: pages are committed only as the stack grows.
const STACK_SIZE: usize = 8 * 1024 * 1024;

fn main() {
    let command = std::thread::Builder::new()
        .name("wsblast".to_string())
        .stack_size(STACK_SIZE)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to start the async runtime")
                .block_on(wsblast::__private::run_cli());
        })
        .expect("failed to start the command thread");
    // A panic was already printed by the panic hook; exit as a panicking
    // main thread would.
    if command.join().is_err() {
        std::process::exit(101);
    }
}
