use std::io::Write;
use std::process::Command;

use vani::{compile_to_c, compile_to_llvm};

fn cc_available() -> bool {
    Command::new("cc")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn lli_available() -> bool {
    Command::new("lli")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn compile_and_run_c(c_source: &str, tag: &str) -> (i32, String) {
    let dir = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let c_path = dir.join(format!("intent-userdrop-{}-{}.c", tag, nanos));
    let bin_path = dir.join(format!("intent-userdrop-{}-{}.bin", tag, nanos));
    {
        let mut f = std::fs::File::create(&c_path).expect("write c");
        f.write_all(c_source.as_bytes()).expect("write");
    }
    let status = Command::new("cc")
        .arg(&c_path)
        .arg("-o")
        .arg(&bin_path)
        .status()
        .expect("cc runs");
    assert!(
        status.success(),
        "cc failed on {} — generated C:\n{}",
        tag,
        c_source
    );
    let run = Command::new(&bin_path).output().expect("binary runs");
    let _ = std::fs::remove_file(&c_path);
    let _ = std::fs::remove_file(&bin_path);
    (
        run.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&run.stdout).to_string(),
    )
}

fn run_with_lli(ll_source: &str, tag: &str) -> (i32, String) {
    let dir = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ll_path = dir.join(format!("intent-userdrop-{}-{}.ll", tag, nanos));
    {
        let mut f = std::fs::File::create(&ll_path).expect("write ll");
        f.write_all(ll_source.as_bytes()).expect("write");
    }
    let run = Command::new("lli")
        .arg(&ll_path)
        .output()
        .expect("lli runs");
    let _ = std::fs::remove_file(&ll_path);
    (
        run.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&run.stdout).to_string(),
    )
}

const SOURCE: &str = r#"
struct Buffer { name: OwnedStr, items: Vec<i64> }

interface Drop {
  fn drop(self: mut ref Buffer) -> i64;
}

implement Drop for Buffer {
  fn drop(self: mut ref Buffer) -> i64 {
    print "dropping", self.name;
    return 0;
  }
}

fn main() -> i64 {
  let b: Buffer = Buffer { name: "buf-1" + "", items: vec(10, 20, 30) };
  return len(b.items) as i64;
}
"#;

#[test]
fn user_drop_by_mut_ref_runs_before_per_field_drops_tree_c() {
    if !cc_available() {
        return;
    }
    let c = compile_to_c(SOURCE).expect("Epic C compiles to C");
    let (code, stdout) = compile_and_run_c(&c, "epicc_c");
    assert_eq!(code, 3, "expected len(b.items) == 3, got {}", code);
    assert!(
        stdout.contains("dropping buf-1"),
        "expected user-Drop to run; stdout was {:?}",
        stdout
    );
}

#[test]
fn user_drop_by_mut_ref_runs_before_per_field_drops_llvm() {
    if !lli_available() {
        return;
    }
    let ll = compile_to_llvm(SOURCE).expect("Epic C compiles to LLVM");
    let (code, stdout) = run_with_lli(&ll, "epicc_llvm");
    assert_eq!(code, 3, "expected len(b.items) == 3, got {}", code);
    assert!(
        stdout.contains("dropping buf-1"),
        "expected user-Drop to run via lli; stdout was {:?}",
        stdout
    );
}
