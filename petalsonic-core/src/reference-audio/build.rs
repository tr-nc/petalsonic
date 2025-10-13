use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=assets/*");

    let src_dir = "assets";
    let out_dir = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("assets");
    copy_directory(src_dir, out_dir).unwrap();
}

fn copy_directory(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&dst)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_directory(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }

    Ok(())
}
