fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        for file in ["mousehop.rc", "app.ico", "tray-light.ico", "tray-dark.ico"] {
            println!("cargo:rerun-if-changed=assets/windows/{file}");
        }
        embed_resource::compile("assets/windows/mousehop.rc", embed_resource::NONE)
            .manifest_required()
            .expect("compile Windows icon resources");
    }
}
