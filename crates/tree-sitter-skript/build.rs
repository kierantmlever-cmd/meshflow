//! Compile the generated tree-sitter parser.
//!
//! `src/parser.c` is machine output from `tree-sitter generate`; regenerate it from `grammar.js`
//! rather than editing it.

fn main() {
    let src = std::path::Path::new("src");
    println!("cargo:rerun-if-changed=src/parser.c");
    println!("cargo:rerun-if-changed=grammar.js");

    cc::Build::new()
        .include(src)
        // The generated parser trips these on every grammar; they are not ours to fix.
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .file(src.join("parser.c"))
        .compile("tree-sitter-skript");
}
