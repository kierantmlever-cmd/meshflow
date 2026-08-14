//! Skript grammar for tree-sitter.
//!
//! Derived from the [Sk-VSC](https://github.com/AyhamAl-Ali/Sk-VSC) TextMate grammar. See
//! `grammar.js` for what is mirrored and what is deliberately simplified.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_skript() -> *const ();
}

/// The grammar, for `tree_sitter::Parser::set_language`.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_skript) };

/// Highlight captures, resolved against `freya-code-editor`'s theme slots.
pub const HIGHLIGHTS_QUERY: &str = include_str!("../queries/highlights.scm");

#[cfg(test)]
mod tests {
    #[test]
    fn the_grammar_loads_and_parses() {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&super::LANGUAGE.into()).expect("grammar loads");

        // A file with a bit of everything, including a string containing a keyword — the case
        // that lexed as three separate nodes before the string tokens got their precedence.
        let tree = parser
            .parse("on join:\n    send \"&atick %player%\" to player\n", None)
            .expect("parses");
        assert!(!tree.root_node().has_error(), "unexpected parse error: {}", tree.root_node());
        assert_eq!(tree.root_node().child(0).unwrap().kind(), "event");
    }
}
