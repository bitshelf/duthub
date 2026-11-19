# 主题 token 规格（自动提取）

> GPUI 深色/浅色主题的移植对照；变量名即语义。由 `scripts/extract-ui-spec.pl` 生成。
>
> **手改注记（2026-11-23）**：生成器 `scripts/extract-ui-spec.pl:3` 的输入指向仓库根 `duthub.html`（该路径不存在；现存资产在 `docs/duthub.html`），无法重跑再生成——本文件本轮**手改**：Dark 段按 `docs/duthub.html:10` 与 `apps/desktop/src/theme.rs:160-161`（radius 6.0 / radius_lg 10.0）双实测对齐，修正旧 `--radius:12px`，补录已落地 token `--accent-contrast`/`--backdrop`/`--accent-ring`/`--sidebar`；脚本修复方式留 owner 裁量。Light 段未在本轮改动范围。

## Dark（默认）

```css
:root{--bg:#0a1019;--rail:#0c131d;--surface:#101926;--panel:#121d2b;--raised:#182638;--hover:#1e2c3d;--border:#263445;--line:#1e2b3b;--text:#e8f0f7;--muted:#8ca0b5;--faint:#5f758b;--accent:#56cbb9;--accent-weak:#113d3c;--accent-contrast:#071b1b;--blue:#88b7ed;--warn:#e9ba78;--red:#ec8291;--term:#090f18;--term-fg:#cfdbe8;--prompt:#71ddbf;--match:#edbe7a;--backdrop:#030911ad;--accent-ring:#48cbb91a;--sidebar:#0e1520;--shadow:0 16px 60px #0004;--radius:6px;--radius-lg:10px;font-synthesis:none}
```

## Light

```css
:root[data-theme=light]{--bg:#f4f7fb;--rail:#eaf0f6;--surface:#ffffff;--panel:#f8fbff;--raised:#edf3fa;--hover:#e2ebf4;--border:#d3dee9;--line:#e2eaf2;--text:#182c40;--muted:#63798c;--faint:#8495a6;--accent:#167e75;--accent-weak:#d8f2ed;--blue:#1766ac;--warn:#a66e1d;--red:#a93e55;--shadow:0 14px 48px #465b721c;--term:#fbfdff;--term-fg:#244058;--prompt:#13826b;--match:#a16b11}
```
