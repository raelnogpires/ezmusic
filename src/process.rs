//! Resource-bounded subprocess helpers.

use std::{
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub(crate) struct CapturedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(crate) fn output_limited(
    mut command: Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<CapturedOutput> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    configure_process_group(&mut command);
    let mut child = command.spawn().context("falha ao iniciar subprocesso")?;
    let stdout = child.stdout.take().context("stdout indisponivel")?;
    let stderr = child.stderr.take().context("stderr indisponivel")?;
    let stdout_reader = thread::spawn(move || read_limited(stdout, stdout_limit));
    let stderr_reader = thread::spawn(move || read_limited(stderr, stderr_limit));
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate_process_group(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(error).context("falha ao consultar subprocesso");
            }
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!("subprocesso excedeu o limite de {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(25));
    };

    let stdout = join_reader(stdout_reader, "stdout")?;
    let stderr = join_reader(stderr_reader, "stderr")?;
    Ok(CapturedOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_limited(reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut output)?;
    if output.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("saida excedeu o limite de {limit} bytes"),
        ));
    }
    Ok(output)
}

fn join_reader(handle: thread::JoinHandle<io::Result<Vec<u8>>>, stream: &str) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("thread de leitura de {stream} terminou inesperadamente"))?
        .with_context(|| format!("falha ao limitar {stream}"))
}

pub(crate) fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);

        #[cfg(target_os = "linux")]
        unsafe {
            // Prevent detached TUI tasks from leaving media tools behind when the
            // EzMusic process exits. `prctl` is a direct syscall suitable here.
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

pub(crate) fn terminate_process_group(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        // Every managed child starts a new process group whose id is the child pid.
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn caps_captured_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 123456789"]);
        let error = output_limited(command, Duration::from_secs(1), 4, 4).unwrap_err();
        assert!(error.to_string().contains("limitar stdout"));
    }

    #[test]
    fn times_out_child_process_group() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10"]);
        let error = output_limited(command, Duration::from_millis(50), 16, 16).unwrap_err();
        assert!(error.to_string().contains("excedeu o limite"));
    }
}
