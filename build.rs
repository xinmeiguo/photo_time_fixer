//! 把应用图标与版本信息编进 exe 的资源节。
//!
//! 走 GNU 工具链（见 `rust-toolchain.toml`）的 `windres`：先把 `assets/app.rc`
//! 编成 COFF 对象，再用 `cargo:rustc-link-arg-bins` 交给链接器。
//! Windows 上 mingw 的 binutils 就带 windres；交叉编译时可用环境变量
//! `WINDRES` 指定（如 `x86_64-w64-mingw32-windres`）。

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/icon_128.rgba");
    println!("cargo:rerun-if-changed=assets/app.rc");

    // 只有 Windows 目标才有资源节
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let windres = std::env::var("WINDRES").unwrap_or_else(|_| "windres".to_string());
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR 缺失");
    let obj = format!("{out_dir}/app_res.o");

    let status = Command::new(&windres)
        .arg("--input-format=rc")
        .arg("--output-format=coff")
        // app.rc 是 UTF-8 且含中文（产品名/文件描述），必须声明代码页，
        // 否则 windres 按 ANSI 解析会把中文变成乱码。
        .arg("--codepage=65001")
        .arg("-I")
        .arg("assets")
        .arg("-i")
        .arg("assets/app.rc")
        .arg("-o")
        .arg(&obj)
        .status()
        .unwrap_or_else(|e| {
            panic!("无法执行 {windres}（Windows 下需把 mingw 的 bin 目录放进 PATH）: {e}")
        });
    assert!(status.success(), "windres 编译资源失败");

    println!("cargo:rustc-link-arg-bins={obj}");
}
