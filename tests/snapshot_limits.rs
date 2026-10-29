use std::fs;
use std::path::PathBuf;
use ward_node_manager::workspace_snapshot::{decode, encode, encode_directory, WorkspaceFile};

fn checkout(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "wardnm-snapshot-limits-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("create checkout");
    directory
}

#[test]
fn packs_a_repository_sized_checkout() {
    let directory = checkout("checkout");
    for module in 0..40 {
        let module_directory = directory.join(format!("src/module{module}"));
        fs::create_dir_all(&module_directory).expect("create module");
        for unit in 0..40 {
            fs::write(
                module_directory.join(format!("unit{unit}.rs")),
                b"fn f() {}\n",
            )
            .expect("write unit");
        }
    }
    fs::write(directory.join("vendor.bin"), vec![7; 3 * 1024 * 1024]).expect("write vendored blob");

    let snapshot =
        encode_directory(&directory).expect("a 1,601 file checkout with a 3 MiB blob packs");
    let files = decode(&snapshot).expect("decode");
    assert_eq!(files.len(), 1_601);
    assert_eq!(
        files.iter().map(|file| file.contents.len()).max(),
        Some(3 * 1024 * 1024)
    );
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn accepts_a_checkout_up_to_half_a_gibibyte_and_refuses_more() {
    let blob = |index: u32, bytes: usize| WorkspaceFile {
        path: format!("blob{index}"),
        contents: vec![0; bytes],
    };
    let within = (0..7)
        .map(|index| blob(index, 64 * 1024 * 1024))
        .collect::<Vec<_>>();
    assert!(encode(&within).is_ok());

    let over = (0..9)
        .map(|index| blob(index, 64 * 1024 * 1024))
        .collect::<Vec<_>>();
    assert!(encode(&over).is_err());
    assert!(encode(&[blob(0, 64 * 1024 * 1024 + 1)]).is_err());
}
