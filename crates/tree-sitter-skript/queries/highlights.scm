; Highlight captures for Skript.
;
; The capture names are not arbitrary — freya-code-editor resolves each one to a field on
; `EditorSyntaxTheme`, and that palette has a fixed set of slots. So these names are chosen for
; which *colour slot* they land in, not for what they would mean in a conventional language. The
; Sk-VSC scope each one stands in for is named beside it, and `mf_ui::skript` fills the matching
; slot with Sk-VSC's own colour.
;
; Where two Sk-VSC scopes share a colour they share a slot here too.

; comment.line.number-sign.skript
(comment) @comment
; comment.line.number-sign.important.skript (#!) — `comment` and `comment.documentation` resolve
; to the same field, so the two emphasised comment forms have to borrow other slots.
(comment_note) @text.literal
; comment.line.number-sign.important.two.skript (#!!)
(comment_todo) @text.reference

; string.quoted.double.skript
(string) @string
(string_text) @string
; string.quoted.double.variable.skript — `%expr%` inside a string
(interpolation_inner) @string.special
; string.quoted.double.options.skript — `{@option}` inside a string
(option_ref_inner) @text.title
(variable_inner) @variable

; variable.other.skript
(variable) @variable
; string.quoted.double.options.skript
(option_ref) @text.title
; variable.external.skript — `%expr%` outside a string
(interpolation) @variable.builtin

; keyword.section.skript
(event) @function
; keyword.command.skript
(command) @text.title
; keyword.options.skript
(section) @text.uri
; keyword.section.meta.skript
(meta_key) @function.method

; keyword.control.skript
(control) @keyword
; keyword.stop.skript
(stop) @punctuation.special
; keyword.effect.skript
(effect) @constant
; keyword.expressions.skript — same colour as effects in Sk-VSC
(expression) @property
; keyword.control.others.skript
(connector) @module
; keyword.types.skript and keyword.commandargs.skript
(type) @type
; entity.playerobjects.skript
(player_object) @variable.parameter
; keyword.loopobjects.skript
(loop_object) @label
; keyword.gui.skript
(gui) @attribute
; keyword.inventory.expression.skript
(inventory) @tag
; keyword.time.skript
(time_unit) @text.emphasis

; keyword.control.boolean.true / .false — two different colours in Sk-VSC, and `boolean` is a
; single slot, so `false` borrows another.
(boolean_true) @boolean
(boolean_false) @escape

; keyword.now.skript
(number) @number
; keyword.operator.skript
(operator) @operator
; keywork.operator.borders.skript [sic — the typo is Sk-VSC's]
(border) @punctuation.bracket

; skript.color.* — Sk-VSC gives each of the 16 Minecraft codes its own literal colour. The
; palette has no room for 16 more slots, so they share one. See the note in mf_ui::skript.
(color_code) @punctuation.delimiter
(color_code_bare) @punctuation.delimiter
(color_name) @punctuation.delimiter
