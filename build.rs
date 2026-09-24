use std::path::Path;

fn main() {
    // secrets.toml подключается в src/config.rs через include_str! — без него сборка невозможна
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR не задан");
    if !Path::new(&manifest_dir).join("secrets.toml").exists() {
        panic!(
            "secrets.toml не найден: скопируйте secrets.toml.example в secrets.toml \
             и заполните данные Wi-Fi"
        );
    }

    embuild::espidf::sysenv::output();
}
