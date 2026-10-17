use super::*;

impl Runtime {
    pub fn logs(
        &self,
        name: &InstanceName,
        service: Option<&str>,
        follow: bool,
        tail: Option<u64>,
        head: Option<u64>,
    ) -> Result<()> {
        let service = service.unwrap_or("app");
        let container = format!("{}-{service}", prefix(name));
        if let Some(lines) = head {
            return print_log_head(&container, lines);
        }
        let args = logs_args(&container, follow, tail);
        stream_docker_logs(&args)
    }
}

fn stream_docker_logs(args: &[String]) -> Result<()> {
    let status = docker_cli().args(args).status().context("running Docker")?;
    if docker_logs_failed(&status) {
        bail!("docker {} failed", args.join(" "));
    }
    Ok(())
}

/// The reader closing the pipe is not a failed `docker logs`. The process is
/// killed by SIGPIPE, or it exits with the shell's `128 + SIGPIPE` status.
pub(super) fn docker_logs_failed(status: &std::process::ExitStatus) -> bool {
    !status.success() && !reader_closed(status)
}

fn reader_closed(status: &std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(libc::SIGPIPE) || status.code() == Some(128 + libc::SIGPIPE)
    }
    #[cfg(not(unix))]
    {
        // 128 + SIGPIPE. SIGPIPE is 13 on the Unix targets this launcher runs.
        status.code() == Some(141)
    }
}

pub(super) fn logs_args(container: &str, follow: bool, tail: Option<u64>) -> Vec<String> {
    let mut args = vec!["logs".to_owned()];
    if follow {
        args.push("--follow".to_owned());
    }
    if let Some(lines) = tail {
        args.extend(["--tail".to_owned(), lines.to_string()]);
    }
    args.push(container.to_owned());
    args
}

/// docker has no head option, so stream both of its output streams through
/// one pipe and stop the `docker logs` process once enough lines have been printed.
pub(super) fn print_log_head(container: &str, lines: u64) -> Result<()> {
    let (reader, writer) = std::io::pipe().context("creating log pipe")?;
    let mut child = Command::new("docker")
        .args(["logs", container])
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .spawn()
        .context("running Docker")?;
    let printed = match copied_log_lines(copy_lines(
        std::io::BufReader::new(reader),
        std::io::stdout().lock(),
        lines,
    ))? {
        LogCopy::ReaderClosed => {
            // stdout's reader is gone. Stop the producer and do not report it.
            stop_log_child(&mut child)?;
            return Ok(());
        }
        LogCopy::Printed(printed) => printed,
    };
    if printed == lines {
        child.kill().context("stopping docker logs")?;
    }
    // a killed `docker logs` leaves no exit code, so only its own failures count
    if child.wait()?.code().is_some_and(|code| code != 0) {
        bail!("docker logs {container} failed");
    }
    Ok(())
}

#[derive(Debug)]
pub(super) enum LogCopy {
    Printed(u64),
    ReaderClosed,
}

/// `ReaderClosed` is a broken pipe on the way out. Other write errors still fail.
pub(super) fn copied_log_lines(result: std::io::Result<u64>) -> Result<LogCopy> {
    match result {
        Ok(printed) => Ok(LogCopy::Printed(printed)),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(LogCopy::ReaderClosed),
        Err(error) => Err(error.into()),
    }
}

fn stop_log_child(child: &mut Child) -> Result<()> {
    if child.try_wait().context("checking docker logs")?.is_none() {
        match child.kill() {
            Ok(()) => {}
            Err(error) => {
                if child.try_wait().context("checking docker logs")?.is_none() {
                    return Err(error).context("stopping docker logs");
                }
            }
        }
    }
    child.wait().context("waiting for docker logs")?;
    Ok(())
}

pub(super) fn copy_lines(
    mut reader: impl std::io::BufRead,
    mut out: impl std::io::Write,
    limit: u64,
) -> std::io::Result<u64> {
    let mut line = Vec::new();
    let mut copied = 0;
    while copied < limit {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        out.write_all(&line)?;
        copied += 1;
    }
    out.flush()?;
    Ok(copied)
}
