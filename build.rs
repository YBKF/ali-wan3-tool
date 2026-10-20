fn main() {
    #[cfg(target_os = "windows")]
    {
        use std::{env, path::Path};

        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let icon_path = Path::new(manifest_dir)
            .join("./assets/icons/app.ico")
            .canonicalize()
            .expect("icon file not exist");

        let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
        println!("cargo:rustc-env=APP_VERSION={}", version);

        let mut res = winresource::WindowsResource::new();
        res.set_icon(&icon_path.to_string_lossy())
            .set("CompanyName", "YBKF")
            .set("FileDescription", "通义万相 3.0 视频生成工具")
            .set("InternalName", "ali-wan3-tool")
            .set("FileVersion", &version);
        res.compile().unwrap();
    }
}
