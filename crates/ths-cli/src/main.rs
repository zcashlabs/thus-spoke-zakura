fn main() -> anyhow::Result<std::process::ExitCode> {
    end_with_the_reader();
    match thus_spoke_zakura::run_from(std::env::args_os()) {
        Ok(code) => Ok(code),
        Err(error) => match error.downcast::<clap::Error>() {
            Ok(error) => error.exit(),
            Err(error) => Err(error),
        },
    }
}

/// Rust starts with SIGPIPE ignored, so once the reader of `ths list | head -1`
/// was gone every later print panicked with "failed printing to stdout" or
/// failed with "Broken pipe". Restore the default disposition: the process
/// ends quietly, the way `docker logs` does.
#[cfg(unix)]
fn end_with_the_reader() {
    // SAFETY: changes one signal disposition before any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn end_with_the_reader() {}
