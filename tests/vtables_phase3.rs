use std::io::Write;
use std::process::Command;

use vani::compile_to_c;
use vani::compile_to_llvm;

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

fn lli_available() -> bool {
    Command::new("lli")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_with_lli(ll_source: &str, tag: &str) -> i32 {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ll_path = dir.join(format!("intent-vtbl-{}-{}-{}.ll", tag, pid, nanos));
    {
        let mut f = std::fs::File::create(&ll_path).expect("write ll");
        f.write_all(ll_source.as_bytes()).expect("write");
    }
    let run = Command::new("lli")
        .arg(&ll_path)
        .status()
        .expect("lli runs");
    let _ = std::fs::remove_file(&ll_path);
    run.code().unwrap_or(-1)
}

#[test]
fn ref_dyn_iface_dispatches_via_vtable_tree_c() {
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

        fn area_of(d: ref dyn Drawable) -> i64 {
          return d.area();
        }

        fn main() -> i64 {
          let c: Circle = Circle { r: 6 };
          let dyn_c: dyn Drawable = c;
          return area_of(ref dyn_c);
        }
    "#;
    let c = compile_to_c(source).expect("ref dyn compiles to C");
    let code = compile_and_run(&c, "ref_dyn_c");
    assert_eq!(
        code, 36,
        "expected area(Circle {{ r: 6 }}) == 36 via ref dyn, got {}",
        code
    );
}

#[test]
fn ref_dyn_iface_dispatches_via_vtable_llvm() {
    if !lli_available() {
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

        fn area_of(d: ref dyn Drawable) -> i64 {
          return d.area();
        }

        fn main() -> i64 {
          let c: Circle = Circle { r: 6 };
          let dyn_c: dyn Drawable = c;
          return area_of(ref dyn_c);
        }
    "#;
    let ll = compile_to_llvm(source).expect("ref dyn compiles to LLVM");
    let code = run_with_lli(&ll, "ref_dyn_llvm");
    assert_eq!(
        code, 36,
        "expected area(Circle {{ r: 6 }}) == 36 via ref dyn (LLVM), got {}",
        code
    );
}

#[test]
fn vec_of_dyn_iface_iterates_with_polymorphic_dispatch_tree_c() {
    if !cc_available() {
        return;
    }
    let source = r#"
        struct Circle { r: i64 }
        struct Square { side: i64 }

        interface Drawable {
          fn area(self: Circle) -> i64;
        }

        implement Drawable for Circle {
          fn area(self: Circle) -> i64 { return self.r * self.r; }
        }

        implement Drawable for Square {
          fn area(self: Square) -> i64 { return self.side * self.side; }
        }

        fn main() -> i64 {
          let c: Circle = Circle { r: 3 };
          let s: Square = Square { side: 5 };
          let xs: Vec<dyn Drawable> = vec(c, s);
          let total: i64 = 0;
          for x in xs { total = total + x.area(); }
          return total;
        }
    "#;
    let c = compile_to_c(source).expect("Vec<dyn> compiles to C");
    let code = compile_and_run(&c, "vec_dyn_c");
    assert_eq!(
        code, 34,
        "expected Circle(3).area + Square(5).area == 9 + 25 == 34, got {}",
        code
    );
}

#[test]
fn vec_of_dyn_iface_iterates_with_polymorphic_dispatch_llvm() {
    if !lli_available() {
        return;
    }
    let source = r#"
        struct Circle { r: i64 }
        struct Square { side: i64 }

        interface Drawable {
          fn area(self: Circle) -> i64;
        }

        implement Drawable for Circle {
          fn area(self: Circle) -> i64 { return self.r * self.r; }
        }

        implement Drawable for Square {
          fn area(self: Square) -> i64 { return self.side * self.side; }
        }

        fn main() -> i64 {
          let c: Circle = Circle { r: 3 };
          let s: Square = Square { side: 5 };
          let xs: Vec<dyn Drawable> = vec(c, s);
          let total: i64 = 0;
          for x in xs { total = total + x.area(); }
          return total;
        }
    "#;
    let ll = compile_to_llvm(source).expect("Vec<dyn> compiles to LLVM");
    let code = run_with_lli(&ll, "vec_dyn_llvm");
    assert_eq!(
        code, 34,
        "expected Circle(3).area + Square(5).area == 9 + 25 == 34 via lli, got {}",
        code
    );
}

#[test]
fn dyn_struct_field_dispatches_via_vtable_tree_c() {
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

        struct Holder { d: dyn Drawable }

        fn use_holder(h: Holder) -> i64 {
          return h.d.area();
        }

        fn main() -> i64 {
          let c: Circle = Circle { r: 4 };
          let h: Holder = Holder { d: c };
          return use_holder(h);
        }
    "#;
    let c = compile_to_c(source).expect("dyn-field struct compiles to C");
    let code = compile_and_run(&c, "dyn_struct_field_c");
    assert_eq!(
        code, 16,
        "expected area(Circle {{ r: 4 }}) == 16 via struct field dispatch, got {}",
        code
    );
}

#[test]
fn dyn_struct_field_dispatches_via_vtable_llvm() {
    if !lli_available() {
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

        struct Holder { d: dyn Drawable }

        fn use_holder(h: Holder) -> i64 {
          return h.d.area();
        }

        fn main() -> i64 {
          let c: Circle = Circle { r: 4 };
          let h: Holder = Holder { d: c };
          return use_holder(h);
        }
    "#;
    let ll = compile_to_llvm(source).expect("dyn-field struct compiles to LLVM");
    let code = run_with_lli(&ll, "dyn_struct_field_llvm");
    assert_eq!(
        code, 16,
        "expected area(Circle {{ r: 4 }}) == 16 via struct field dispatch (LLVM), got {}",
        code
    );
}

#[test]
fn dyn_dispatch_returns_value_via_vtable_indirect_call_llvm() {
    if !lli_available() {
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
    let ll = compile_to_llvm(source).expect("dyn dispatch compiles to LLVM IR");
    assert!(
        ll.contains("%intent_vtbl_Drawable = type"),
        "expected vtable typedef in LLVM IR:\n{ll}"
    );
    assert!(
        ll.contains("@intent_vtbl_Drawable_Circle = constant"),
        "expected global vtable constant:\n{ll}"
    );
    assert!(
        ll.contains("@intent_trampoline_Circle_Drawable_0_area"),
        "expected trampoline definition:\n{ll}"
    );
    let code = run_with_lli(&ll, "dyn_area_llvm");
    assert_eq!(
        code, 25,
        "expected area_of_dyn(Circle {{ r: 5 }}) == 25 via lli, got exit {} — LLVM IR:\n{}",
        code, ll
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
