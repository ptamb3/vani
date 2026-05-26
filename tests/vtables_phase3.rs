use std::io::Write;
use std::process::Command;

use vani::compile_to_c;

fn cc_available() -> bool {
    Command::new("cc")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn compile_and_run(c_source: &str, tag: &str) -> i32 {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let c_path = dir.join(format!("intent-vtbl-{}-{}-{}.c", tag, pid, nanos));
    let bin_path = dir.join(format!("intent-vtbl-{}-{}-{}.bin", tag, pid, nanos));
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
    let run = Command::new(&bin_path).status().expect("binary runs");
    let _ = std::fs::remove_file(&c_path);
    let _ = std::fs::remove_file(&bin_path);
    run.code().unwrap_or(-1)
}

#[test]
fn dyn_dispatch_returns_value_via_vtable_indirect_call() {
    if !cc_available() {
        return;
    }
    let source = r#"
        struct Circle { r: i64 }

        interface Drawable {
          fn area(self: Circle) -> i64;
        }

        implement Drawable for Circle {
          fn area(self: Circle) -> i64 { return self.r * self.r; }
        }

        fn area_of_dyn(d: dyn Drawable) -> i64 {
          return d.area();
        }

        fn main() -> i64 {
          let c: Circle = Circle { r: 5 };
          return area_of_dyn(c);
        }
    "#;
    let c = compile_to_c(source).expect("dyn dispatch compiles to C");
    let code = compile_and_run(&c, "dyn_area");
    assert_eq!(
        code, 25,
        "expected area_of_dyn(Circle {{ r: 5 }}) == 25, got exit {} — generated C:\n{}",
        code, c
    );
}

#[test]
fn dyn_dispatch_emits_per_iface_typedefs_and_static_vtable() {
    let source = r#"
        struct Point { x: i64 }

        interface Movable {
          fn step(self: Point) -> i64;
        }

        implement Movable for Point {
          fn step(self: Point) -> i64 { return self.x + 1; }
        }

        fn first(d: dyn Movable) -> i64 { return d.step(); }

        fn main() -> i64 {
          let p: Point = Point { x: 41 };
          return first(p);
        }
    "#;
    let c = compile_to_c(source).expect("dyn typedefs compile");
    assert!(
        c.contains("typedef struct intent_vtbl_Movable"),
        "expected intent_vtbl_Movable typedef in:\n{c}"
    );
    assert!(
        c.contains("typedef struct intent_dyn_Movable"),
        "expected intent_dyn_Movable typedef in:\n{c}"
    );
    assert!(
        c.contains("intent_vtbl_Movable_Point"),
        "expected per-(T, Iface) static vtable name in:\n{c}"
    );
    assert!(
        c.contains("intent_trampoline_Point_Movable_0_step"),
        "expected trampoline name in:\n{c}"
    );
}
