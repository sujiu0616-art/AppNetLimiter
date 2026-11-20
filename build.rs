fn main() {
    // 资源脚本：程序图标（1 ICON）+ 管理员权限 manifest（1 24）
    // exe 启动时自动弹 UAC；资源管理器/任务栏显示自定义图标
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/app.ico");
    let _ = embed_resource::compile("app.rc", embed_resource::NONE);
}
