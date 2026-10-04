#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::process::Command;

#[tokio::test]
async fn process_group_zero_makes_child_its_own_group_leader() {
    let mut cmd = Command::new("sleep");
    cmd.arg("30")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = cmd.spawn().expect("spawn sleep");
    let pid = child.id().expect("child has pid") as i32;

    let pgid = unsafe { libc::getpgid(pid) };
    assert_eq!(pgid, pid, "child must be its own process group leader");

    killpg(Pid::from_raw(pid), Signal::SIGKILL).ok();
    let _ = child.wait().await;
}

#[tokio::test]
async fn killpg_reaches_grandchildren_in_the_same_group() {
    // Shell forks a background sleep (the "grandchild") then execs
    // its own sleep; both inherit the group id set by process_group.
    // One killpg must terminate both.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("sleep 30 & echo $! >&2; exec sleep 30")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn sh");
    let pid = child.id().expect("child pid") as i32;

    let stderr = child.stderr.take().expect("stderr piped");
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stderr));
    let grandchild_pid: i32 = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("read grandchild pid")
        .expect("read line ok")
        .expect("line present")
        .trim()
        .parse()
        .expect("parse pid");

    killpg(Pid::from_raw(pid), Signal::SIGKILL).expect("killpg");
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child reaped");

    // Give the kernel a moment to deliver and the grandchild's
    // parent (sh, now dead) to reparent it to PID 1, which then
    // reaps it. `kill -0` reports ESRCH once gone.
    for _ in 0..50 {
        if unsafe { libc::kill(grandchild_pid, 0) } < 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("grandchild {grandchild_pid} still alive after killpg");
}
