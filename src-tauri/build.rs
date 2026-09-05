fn main() {
    let build_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    println!("cargo:rustc-env=CODEXFF_BUILD_AT_UNIX={build_at_unix}");
    tauri_build::build()
}
