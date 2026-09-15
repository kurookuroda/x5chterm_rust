//! 多重起動防止(ファイルロック)。Python版 lock.py に対応。
//!
//! 既にロックされている場合(=バックグラウンドの転送プロセスが稼働中)、
//! PIDファイルからそのプロセスをTERM→(5秒待機)→KILLの順で停止させてから
//! ロックを取得し直す「ハンドオフ」を行う。
//!
//! flock自体は標準ライブラリにないので `fs2` クレート(FileExt::lock_exclusive等)を、
//! シグナル送信は `nix` クレートを使う。どちらもPythonでいう fcntl / os.kill /
//! signal モジュールに相当する薄いOSラッパー。

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Duration;

use fs2::FileExt;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LockError(pub String);

/// ロックファイルのファイルハンドルを返す。呼び出し元はプロセス終了までこのハンドルを
/// 保持し続けること(dropされるとロックが解放される。Pythonのファイルオブジェクトが
/// close/GCされるとロックが解けるのと同じ仕組み)。
pub fn acquire_lock_with_handoff(lock_path: &str, pid_path: &str) -> Result<File, LockError> {
    use std::io::Write;

    let lock_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(lock_path)
        .map_err(|e| LockError(format!("ロックファイルを開けません: {e}")))?;

    if lock_file.try_lock_exclusive().is_ok() {
        return Ok(lock_file); // 即座に取得できた(通常ケース)
    }

    println!("\x1b[33m[Notice] バックグラウンドで転送プロセス(Cron)が稼働中です。\x1b[0m");

    if let Some(pid) = read_pid(pid_path) {
        if pid > 0 {
            print!("プロセス(PID: {pid})を停止し、処理を引き継ぎます...");
            let _ = std::io::stdout().flush();

            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);

            for _ in 0..5 {
                std::thread::sleep(Duration::from_secs(1));
                if !process_alive(pid) {
                    break;
                }
                print!(".");
                let _ = std::io::stdout().flush();
            }

            if process_alive(pid) {
                print!(" 応答がないため強制終了します(KILL)...");
                let _ = std::io::stdout().flush();
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
    }

    print!(" ロック取得...");
    let _ = std::io::stdout().flush();
    lock_file
        .lock_exclusive()
        .map_err(|e| LockError(format!("ロック取得に失敗しました: {e}")))?;

    println!(" 完了。\n\x1b[32m>> 処理を引き継いで起動します。\x1b[0m");
    std::thread::sleep(Duration::from_secs(1));

    Ok(lock_file)
}

pub fn read_pid(pid_path: &str) -> Option<i32> {
    let content = std::fs::read_to_string(pid_path).ok()?;
    content.trim().parse().ok()
}

/// シグナル0を送ってプロセスの生存を確認する(実際にはシグナルを送らない)。
pub fn process_alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

pub fn write_pid(pid_path: &str) -> std::io::Result<()> {
    std::fs::write(pid_path, std::process::id().to_string())
}

pub fn remove_pid(pid_path: &str) {
    let _ = std::fs::remove_file(pid_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        assert_eq!(read_pid("/tmp/x5ch_rs_no_such_pid_file_xyz"), None);
    }

    #[test]
    fn write_pid_then_read_pid_roundtrips() {
        let path = std::env::temp_dir().join(format!("x5ch_rs_test_pid_{}.pid", std::process::id()));
        let path_str = path.to_string_lossy().into_owned();

        write_pid(&path_str).unwrap();
        assert_eq!(read_pid(&path_str), Some(std::process::id() as i32));

        remove_pid(&path_str);
        assert!(!Path::new(&path_str).exists());
    }

    #[test]
    fn process_alive_is_true_for_self() {
        assert!(process_alive(std::process::id() as i32));
    }

    #[test]
    fn process_alive_is_false_for_unlikely_pid() {
        // PID 999999 のプロセスは通常存在しない(このサンドボックス環境が前提)。
        assert!(!process_alive(999_999));
    }
}
