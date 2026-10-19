# 本机副本是 fork（偏离上游 v2.0.0）

这份源码来自 <https://github.com/macnjil4/egui_sauge> 的 tag `v2.0.0`，
但**不是原样副本** —— 2026-10-12 修掉了一类告警，此后与上游有偏离。

## 改了什么

- 位置：`src/` 下 **35 个文件 / 74 处**；`Cargo.toml`、`Cargo.lock` 保持上游原样。
- 内容：全部是浮点字面量补后缀，`Stroke::new(1.0, …)` → `Stroke::new(1.0_f32, …)`。
  **除 `_f32` 后缀外没有任何别的改动，语义零变化。**
- 动机：上游那 74 条 `float_literal_f32_fallback` 属 `future_incompatible`
  （rust-lang/rust#154024，官方注明**将来会变硬错误**）。上游半年无新提交，等不到。

## 补丁在哪

`D:/test/wrepl/docs/egui_sauge-fork.patch`（随 wrepl 仓库走）。

将来同步上游（换版本 / 重新 clone）**必须重放**：

```bash
cd /d/test/egui_sauge
patch -p1 < /d/test/wrepl/docs/egui_sauge-fork.patch
```

已验证可重放：拿上游 v2.0.0 的原始 `src/` 打上它 → 与本机副本 `diff -rq` 零差异。

## 重跑 cargo fix 的注意事项（本机两个坑）

1. 临时注释掉 `[dev-dependencies]` 里的 `eframe` —— 它默认 feature 会拉 wgpu，
   本机离线缓存里没有，解析直接失败；同时把 `Cargo.lock` 挪走（它的解析结果同样缺那支）。
   修完把两个文件从上游原样还原回来。
2. 本目录**没有** `rust-toolchain.toml`，必须显式指定 GNU 工具链，否则退回默认的
   msvc（本机无 MSVC Build Tools）→ 链接阶段 `link.exe` 失败：

   ```bash
   RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-gnu \
   CARGO_TARGET_DIR=D:/test/cargo-target RUSTC_STAGE=1 \
     cargo fix --lib --allow-no-vcs --offline
   ```

## CI 不受影响

`wrepl` 的发版工作流是 `actions/checkout` 拉上游 v2.0.0，**不走这份 fork** ——
CI 里仍会刷那 74 条警告。等这个 lint 真变成硬错误那天，CI 会先挂。
