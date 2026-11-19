fn main() {
    // Windows：嵌入应用图标（资源 ID 1 —— gpui load_icon 从此处加载）与版本信息
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.set("FileDescription", "Duthub · 串口调试与 MCP Agent 工作台");
        res.set("ProductName", "Duthub");
        res.compile()
            .expect("嵌入 Windows 资源失败（需要 app.ico 存在于 assets/）");
    }
}
