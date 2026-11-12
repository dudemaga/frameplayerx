//! Compiles the WGSL shaders to SPIR-V with naga (pure Rust, no external tools).

use std::path::PathBuf;

/// Each SPIR-V module and the WGSL files joined into it: WGSL has no
/// includes, so shared code is put in front.
const MODULES: [(&str, &[&str]); 6] = [
    ("scene", &["hdr", "scene", "planes"]),
    ("scene_tp10", &["hdr", "scene", "planes_tp10"]),
    ("quad", &["quad"]),
    ("egui", &["egui"]),
    ("q10c", &["q10c"]),
    ("peak", &["hdr", "peak"]),
];

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    for (name, files) in MODULES {
        let mut src = String::new();
        for file in files {
            let path = format!("shaders/{file}.wgsl");
            println!("cargo:rerun-if-changed={path}");
            src += &std::fs::read_to_string(&path).expect("shader source");
            src.push('\n');
        }
        let module = match naga::front::wgsl::parse_str(&src) {
            Ok(m) => m,
            Err(e) => panic!("{name} ({files:?}):\n{}", e.emit_to_string(&src)),
        };
        let info = match naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        {
            Ok(i) => i,
            Err(e) => panic!("{name}: validation failed:\n{}", e.emit_to_string(&src)),
        };
        let options = naga::back::spv::Options {
            lang_version: (1, 3),
            flags: naga::back::spv::WriterFlags::empty(),
            ..Default::default()
        };
        let words = naga::back::spv::write_vec(&module, &info, &options, None)
            .unwrap_or_else(|e| panic!("{name}: SPIR-V generation failed: {e}"));
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        std::fs::write(out.join(format!("{name}.spv")), bytes).expect("write spv");
    }
}
