use std::{env, fs::File, path::PathBuf};

use ico::{IconDir, IconDirEntry, IconImage, ResourceType};
use image::{ImageReader, imageops::FilterType};

fn main() {
    println!("cargo:rerun-if-changed=assets/hand-icon.png");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let source = ImageReader::open("assets/hand-icon.png")
        .expect("app icon PNG must exist")
        .decode()
        .expect("app icon PNG must be readable");
    let mut icon_dir = IconDir::new(ResourceType::Icon);
    for size in [16, 24, 32, 48, 64, 128, 256] {
        let image = source
            .resize_exact(size, size, FilterType::Lanczos3)
            .to_rgba8();
        let icon = IconImage::from_rgba_data(size, size, image.into_raw());
        icon_dir.add_entry(IconDirEntry::encode(&icon).expect("icon frame must encode"));
    }

    let icon_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set")).join("hand.ico");
    icon_dir
        .write(File::create(&icon_path).expect("icon output must be writable"))
        .expect("icon output must be valid");

    let mut resource = winres::WindowsResource::new();
    resource.set_icon(icon_path.to_str().expect("icon path must be UTF-8"));
    resource
        .compile()
        .expect("Windows icon resource must compile");
}
