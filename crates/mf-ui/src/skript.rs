//! Sk-VSC's colours, mapped onto the editor's palette.
//!
//! Taken from [Sk-VSC](https://github.com/AyhamAl-Ali/Sk-VSC) `themes/Sk-VSC.tmTheme.json`. Every
//! constant below is that file's hex value for the named scope, and the field it is assigned to
//! is whichever `EditorSyntaxTheme` slot the matching capture in `queries/highlights.scm` resolves
//! to. Those two files have to be read together — the capture names are the join.
//!
//! **The one place this cannot follow Sk-VSC.** Sk-VSC gives each of the sixteen Minecraft colour
//! codes its own literal colour, so `&c` renders red and `&a` renders green. `EditorSyntaxTheme`
//! is a fixed struct of about thirty colour slots and roughly thirty are already spoken for by the
//! language's own scopes, so there is no room for sixteen more. Every code therefore shares one
//! colour here: still distinct from the string around it, but not the code's own colour. Doing it
//! properly needs a `freya-code-editor` that takes a colour table rather than a fixed struct.

use freya::{code_editor::EditorSyntaxTheme, prelude::Color};

/// Parse `#RRGGBB`, dropping any trailing alpha.
///
/// Sk-VSC writes some colours as `#RRGGBBAA` (`#D885FCD9`). Skia's `Color` here is opaque, and
/// blending against the editor background would be guessing at a background Sk-VSC never had, so
/// the alpha is dropped and the RGB kept.
const fn rgb(hex: u32) -> Color {
    Color::from_rgb(((hex >> 16) & 0xFF) as u8, ((hex >> 8) & 0xFF) as u8, (hex & 0xFF) as u8)
}

// Sk-VSC scope → colour. Names match the scopes in syntaxes/Sk-VSC.json.
const COMMENT: Color = rgb(0x5F5F5F); // comment.line.number-sign
const COMMENT_NOTE: Color = rgb(0xDD5855); // comment.line.number-sign.important        (#!)
const COMMENT_TODO: Color = rgb(0x5D8A3B); // comment.line.number-sign.important.two    (#!!)
const SECTION: Color = rgb(0xE3E3E3); // keyword.section
const SECTION_META: Color = rgb(0xDBDBDB); // keyword.section.meta
const OPERATOR: Color = rgb(0xBDFF4A); // keyword.operator
const CONTROL: Color = rgb(0xD885FC); // keyword.control
const STRING: Color = rgb(0xC3E88D); // string.quoted.double
const STRING_VARIABLE: Color = rgb(0xE5F8FF); // string.quoted.double.variable
const STRING_OPTIONS: Color = rgb(0xFFFFFF); // string.quoted.double.options
const VARIABLE: Color = rgb(0xF8516D); // variable.other
const VARIABLE_EXTERNAL: Color = rgb(0xFFF952); // variable.external
const EFFECT: Color = rgb(0xEDEDED); // keyword.effect
const BOOLEAN_TRUE: Color = rgb(0x55FF55); // keyword.control.boolean.true
const BOOLEAN_FALSE: Color = rgb(0xFF5555); // keyword.control.boolean.false
const STOP: Color = rgb(0xDD5855); // keyword.stop
const NUMBER: Color = rgb(0xF77669); // keyword.now
const PLAYER_OBJECT: Color = rgb(0xF9FF8F); // entity.playerobjects
const BORDER: Color = rgb(0x2BDFFF); // keywork.operator.borders
const EXPRESSION: Color = rgb(0xEDEDED); // keyword.expressions
const CONNECTOR: Color = rgb(0xBA6DE3); // keyword.control.others
const TYPE: Color = rgb(0xFFBC2B); // keyword.types, keyword.commandargs
const LOOP_OBJECT: Color = rgb(0xFFC94C); // keyword.loopobjects
const GUI: Color = rgb(0x4596FF); // keyword.gui
const INVENTORY: Color = rgb(0xDB8CFF); // keyword.inventory.expression
const TIME: Color = rgb(0xFFFFFF); // keyword.time
const OPTIONS: Color = rgb(0xF0F0F0); // keyword.options
const TEXT: Color = rgb(0xD6D6D6); // meta.everything
/// One colour for all of `skript.color.*` — see the note at the top of this file. `&e`'s yellow,
/// because it reads as a marker against both the string green and the background.
const COLOR_CODE: Color = rgb(0xFFF530);

/// Sk-VSC's palette, in the shape the editor wants.
///
/// Starts from `dark()` so any slot this language never uses keeps a sane value rather than
/// whatever `Default` (which is the *light* theme) would leave there.
pub fn syntax_theme() -> EditorSyntaxTheme {
    EditorSyntaxTheme {
        text: TEXT,
        comment: COMMENT,
        text_literal: COMMENT_NOTE,
        text_reference: COMMENT_TODO,

        string: STRING,
        string_special: STRING_VARIABLE,
        // `{@option}` and `command …` are both #FFFFFF in Sk-VSC, so they share a slot.
        text_title: STRING_OPTIONS,
        variable: VARIABLE,
        variable_builtin: VARIABLE_EXTERNAL,

        function: SECTION,
        function_method: SECTION_META,
        text_uri: OPTIONS,

        keyword: CONTROL,
        punctuation_special: STOP,
        constant: EFFECT,
        property: EXPRESSION,
        module: CONNECTOR,
        type_: TYPE,
        variable_parameter: PLAYER_OBJECT,
        label: LOOP_OBJECT,
        attribute: GUI,
        tag: INVENTORY,
        text_emphasis: TIME,

        boolean: BOOLEAN_TRUE,
        escape: BOOLEAN_FALSE,
        number: NUMBER,
        operator: OPERATOR,
        punctuation_bracket: BORDER,
        punctuation_delimiter: COLOR_CODE,

        ..EditorSyntaxTheme::dark()
    }
}

/// Resolve a capture name to its colour the way `freya-code-editor` does.
///
/// Duplicated from that crate because its own resolver is private. Kept only for the tests below:
/// the point is to catch a capture in `highlights.scm` that lands on a slot [`syntax_theme`] never
/// filled, which otherwise shows up as a token that is silently the wrong colour on screen.
#[cfg(test)]
fn resolve(name: &str, theme: &EditorSyntaxTheme) -> Color {
    match name {
        "attribute" => theme.attribute,
        "boolean" => theme.boolean,
        "comment" => theme.comment,
        "constant" => theme.constant,
        "escape" => theme.escape,
        "function" => theme.function,
        "function.method" => theme.function_method,
        "keyword" => theme.keyword,
        "label" => theme.label,
        "module" => theme.module,
        "number" => theme.number,
        "operator" => theme.operator,
        "property" => theme.property,
        "punctuation.bracket" => theme.punctuation_bracket,
        "punctuation.delimiter" => theme.punctuation_delimiter,
        "punctuation.special" => theme.punctuation_special,
        "string" => theme.string,
        "string.special" => theme.string_special,
        "tag" => theme.tag,
        "text.literal" => theme.text_literal,
        "text.reference" => theme.text_reference,
        "text.title" => theme.text_title,
        "text.uri" => theme.text_uri,
        "text.emphasis" => theme.text_emphasis,
        "type" => theme.type_,
        "variable" => theme.variable,
        "variable.builtin" => theme.variable_builtin,
        "variable.parameter" => theme.variable_parameter,
        _ => theme.text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every capture the query assigns, in source order, for one line of Skript.
    fn captures(source: &str) -> Vec<(String, String)> {
        use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

        let mut parser = Parser::new();
        let language: tree_sitter::Language = tree_sitter_skript::LANGUAGE.into();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();

        let query = Query::new(&language, tree_sitter_skript::HIGHLIGHTS_QUERY)
            .expect("highlights.scm is a valid query for this grammar");
        let names = query.capture_names();

        let mut cursor = QueryCursor::new();
        let mut out = Vec::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                out.push((
                    names[cap.index as usize].to_string(),
                    source[cap.node.byte_range()].to_string(),
                ));
            }
        }
        out
    }

    /// The capture assigned to the first token whose text is exactly `text`.
    fn capture_of(source: &str, text: &str) -> String {
        captures(source)
            .into_iter()
            .find(|(_, t)| t == text)
            .map(|(c, _)| c)
            .unwrap_or_else(|| panic!("no capture covered {text:?} in {source:?}"))
    }

    #[test]
    fn the_query_is_valid_against_the_grammar() {
        // A capture naming a node the grammar does not produce is a compile-time-silent typo that
        // only shows as missing colour, so building the query at all is the assertion.
        assert!(!captures("on join:\n").is_empty());
    }

    #[test]
    fn skript_constructs_land_on_the_colours_sk_vsc_gives_them() {
        let theme = syntax_theme();
        let src = "#!! todo\n#! note\n# plain\non join:\n    set {kills::%player%} to 0\n    \
                   send \"&aHi %player%\" to player\n    if {_x} is true:\n        stop\n";

        for (text, expected, what) in [
            ("# plain", COMMENT, "comment"),
            ("#! note", COMMENT_NOTE, "#! comment"),
            ("#!! todo", COMMENT_TODO, "#!! comment"),
            ("on join:", SECTION, "event"),
            ("set", EFFECT, "effect"),
            ("{kills::%player%}", VARIABLE, "variable"),
            ("if", CONTROL, "control keyword"),
            ("true", BOOLEAN_TRUE, "true"),
            ("stop", STOP, "stop"),
            ("to", CONNECTOR, "connector"),
            ("player", PLAYER_OBJECT, "player object"),
            ("&a", COLOR_CODE, "colour code"),
        ] {
            let capture = capture_of(src, text);
            assert_eq!(
                resolve(&capture, &theme),
                expected,
                "{what} ({text:?}) captured as @{capture}, which is not its Sk-VSC colour",
            );
        }
    }

    #[test]
    fn a_string_keeps_its_interpolation_and_colour_codes_apart() {
        let theme = syntax_theme();
        let src = "send \"&aHi %player%\" to player\n";

        // The three pieces Sk-VSC gives three different colours, and the case that regressed
        // while building the grammar: a keyword inside a string used to split the string in two.
        assert_eq!(resolve(&capture_of(src, "&a"), &theme), COLOR_CODE);
        assert_eq!(resolve(&capture_of(src, "%player%"), &theme), STRING_VARIABLE);
        assert_eq!(resolve(&capture_of(src, "Hi "), &theme), STRING);
    }

    #[test]
    fn hex_parses_the_way_sk_vsc_writes_it() {
        assert_eq!(rgb(0xF8516D), Color::from_rgb(0xF8, 0x51, 0x6D));
        assert_eq!(rgb(0xFFFFFF), Color::from_rgb(255, 255, 255));
        assert_eq!(rgb(0x000000), Color::from_rgb(0, 0, 0));
    }

    #[test]
    fn the_two_booleans_stay_distinguishable() {
        let theme = syntax_theme();
        // Sk-VSC colours `true` green and `false` red, and `boolean` is a single slot — so
        // `false` borrows `escape`. If those ever collapse to one colour the query is wrong.
        assert_ne!(theme.boolean, theme.escape);
        assert_eq!(theme.boolean, BOOLEAN_TRUE);
        assert_eq!(theme.escape, BOOLEAN_FALSE);
    }

    #[test]
    fn nothing_is_left_on_the_light_default() {
        let theme = syntax_theme();
        let light = EditorSyntaxTheme::light();
        // The slots this language actually uses must not have fallen through to `Default`, which
        // is the light theme — the same trap that made the editor unreadable in Phase 2.
        for (name, ours) in [
            ("text", theme.text),
            ("comment", theme.comment),
            ("string", theme.string),
            ("variable", theme.variable),
            ("keyword", theme.keyword),
            ("type", theme.type_),
            ("number", theme.number),
            ("operator", theme.operator),
        ] {
            assert_ne!(ours, light.text, "{name} looks like the light default");
        }
    }
}
