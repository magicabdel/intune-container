fn main() {
    // The Tauri build step processes tauri.conf.json and requires the compiled
    // frontend at `frontend/dist`. It is only needed for the desktop interface,
    // so run it solely under the `gui` feature; the default build is CLI-only
    // and needs neither GTK/WebKitGTK nor the frontend assets.
    #[cfg(feature = "gui")]
    tauri_build::build();
}
