/**
 * Skript grammar, derived from the Sk-VSC TextMate grammar
 * (https://github.com/AyhamAl-Ali/Sk-VSC, syntaxes/Sk-VSC.json).
 *
 * This is a *lexer*, not a parser, and that is deliberate. Skript's syntax is natural-language
 * shaped — `on right click on a sign with a diamond:` is one event — so a structural grammar
 * would be inventing a language definition nobody agreed on. Sk-VSC itself is 292 flat regex
 * rules, and highlighting only needs tokens, so this mirrors those rules as tree-sitter tokens
 * and lets the parser accept any sequence of them. That also means it never fails on a file:
 * anything unrecognised falls through to `word` and renders as plain text.
 *
 * The one thing lost against Sk-VSC is `^` anchoring. TextMate matches per line and can require
 * column 0 (events) or leading indentation (effects); tree-sitter tokens cannot. In practice
 * Skript files put these where you would expect, and the tokens that most needed anchoring —
 * events, commands, section headers — are instead pinned by their trailing `:`, which is a
 * stronger signal than the line start anyway.
 */

// Sk-VSC: keyword.control.skript
const CONTROL = [
  'else if', 'if', 'else', 'while', 'loop', 'return', 'continue loop', 'continue', 'for',
  'function', 'contains', 'contain', "doesn't", 'does not', 'does', "isn't", 'is not', 'is',
  "aren't", 'are not', 'are', 'have', 'has',
];

// Sk-VSC: keyword.stop.skript
const STOP = [
  'stop trigger', 'stop loop', 'stop section', 'stop conditionals', 'stop', 'exit trigger',
  'exit loop', 'exit section', 'exit', 'escape', 'cancel event', 'cancel the event',
  'uncancel event', 'uncancel the event', 'break', 'shutdown',
];

// Sk-VSC: keyword.effect.skript — the indented verbs that start a line of behaviour.
const EFFECT = [
  'send message', 'send', 'message', 'broadcast', 'set', 'add', 'give', 'increase',
  'remove all', 'remove every', 'remove', 'subtract', 'reduce', 'delete', 'clear', 'reset',
  'make', 'ban', 'unban', 'ip-ban', 'ip ban', 'kick', 'kill', 'deop', 'op',
  'dye', 'colour', 'color', 'paint', 'open', 'show', 'close', 'reveal', 'hide',
  'damage', 'heal', 'repair', 'feed', 'wait', 'halt', 'drop', 'play sound', 'play',
  'force', 'load', 'loaded', 'unload', 'unloaded', 'rotate', 'say', 'start',
  'enchant', 'disenchant', 'equip', 'wear', 'explosion', 'ignite', 'extinguish',
  'strike lightning', 'set fire to', 'light', 'log', 'poison', 'unpoison', 'cure',
  'push', 'thrust', 'replace all', 'replace every', 'replace', 'shear', 'unshear',
  'shoot', 'let', 'spawn', 'teleport', 'leash', 'unleash', 'lead', 'unlead',
  'toggle', 'switch', 'activate', 'deactivate', 'turn on', 'turn off', 'enable', 'disable',
  'grow', 'create', 'generate', 'connect', 'run command', 'run cmd', 'run',
  'title', 'subtitle', 'action bar', 'actionbar', 'boss bar', 'bossbar',
  'write', 'register', 'unregister', 'call', 'save', 'insert', 'append', 'prepend',
  'assert', 'async', 'sync', 'end', 'bind', 'convert', 'branch', 'fire', 'destroy',
  'download', 'bungeecord connect', 'as op', 'new', 'receive',
];

// Sk-VSC: keyword.expressions.skript
const EXPRESSION = [
  'uncoloured', 'uncolored', 'noncoloured', 'noncolored', 'coloured', 'colored',
  'pitch', 'volume', 'yaw', 'normalized', 'normalize', 'angle', 'between', 'random',
  'squared length', 'velocity', 'around', 'dot', 'cross', 'amount',
  'creative', 'survival', 'spectator', 'adventure', 'named', 'name', 'with name',
  'with lore', 'parsed as', 'parse as', 'and', 'permission', 'permissions',
  'gamerule', 'rule', 'size', 'empty', 'exists', 'exist', 'played', 'play', 'before',
  'server', 'this', 'complete', 'absolute', 'path', 'absorption hearts',
  'accepted items', 'tablist', 'group', 'score', 'all', 'hologram', 'holograms',
  'of file', 'on file',
];

// Sk-VSC: keyword.types.skript and keyword.commandargs.skript share a colour, so they share a
// token here too.
const TYPE = [
  'biome', 'block', 'boolean', 'chunk', 'click type', 'click types', 'colour', 'colours',
  'color', 'colors', 'damage cause', 'damage causes', 'date', 'difficulty', 'difficulties',
  'direction', 'directions', 'enchantment', 'enchantments', 'enchantment type',
  'enchantment types', 'experience', 'gamemode', 'game mode', 'gamemodes', 'game modes',
  'inventory action', 'inventory actions', 'inventory type', 'inventory types',
  'item', 'items', 'itemstack', 'item type', 'item types', 'material', 'materials',
  'living entity', 'living entities', 'livingentity', 'livingentities',
  'location', 'locations', 'money', 'number', 'numbers', 'num', 'nums',
  'object', 'objects', 'offline player', 'offline players', 'offlineplayer',
  'potion effect type', 'potion type', 'projectile', 'recipe', 'region', 'regions',
  'server icon', 'slot type', 'spawn reason', 'teleport cause', 'text', 'texts',
  'string', 'strings', 'time', 'times', 'time period', 'duration', 'durations',
  'timespan', 'time span', 'tree type', 'tree', 'trees', 'vector', 'vectors',
  'visual effect', 'particle effect', 'weather type', 'weather types', 'weather',
  'world', 'worlds', 'integer', 'integers', 'entity type', 'entity types',
  'clear', 'sunny', 'sun', 'rainy', 'raining', 'rain', 'thundering', 'thunderstorm',
  'thunder', 'int', 'byte', 'pnum', 'pinfo', 'short', 'long', 'float', 'array',
  'pjson', 'penum', 'pentity', 'packet', 'packets',
];

// Sk-VSC: entity.playerobjects.skript
const PLAYER_OBJECT = [
  'game mode', 'gamemode', 'all players', 'players', 'player', 'victim', 'attacker',
  'sender', 'loop-player', 'shooter', 'console', 'uuid of', "'s uuid", 'location of',
];

// Sk-VSC: keyword.control.others.skript
const CONNECTOR = ['at', 'by', 'with', 'from', 'to', 'in'];

// Sk-VSC: keyword.time.skript
const TIME_UNIT = [
  'ticks', 'tick', 'seconds', 'second', 'minutes', 'minute', 'hours', 'hour',
  'days', 'day', 'years', 'year',
];

module.exports = grammar({
  name: 'skript',

  // Newlines are whitespace: nothing in this grammar is line-structured, because none of the
  // tokens need to know where a line begins.
  extras: () => [/\s/],

  rules: {
    source_file: $ => repeat($._item),

    _item: $ => choice(
      $.comment_todo,
      $.comment_note,
      $.comment,
      $.string,
      $.option_ref,
      $.variable,
      $.interpolation,
      $.event,
      $.command,
      $.section,
      $.meta_key,
      $.boolean_true,
      $.boolean_false,
      $.stop,
      $.control,
      $.loop_object,
      $.gui,
      $.inventory,
      $.type,
      $.player_object,
      $.time_unit,
      $.effect,
      $.expression,
      $.connector,
      $.color_code_bare,
      $.color_name,
      $.number,
      $.border,
      $.operator,
      $.word,
    ),

    // Sk-VSC gives `#!!` and `#!` their own colours, so they are separate tokens. Longest match
    // orders them correctly without needing precedence.
    comment_todo: () => token(seq('#!!', /[^\n]*/)),
    comment_note: () => token(seq('#!', /[^\n]*/)),
    comment: () => token(seq('#', /[^\n]*/)),

    // Structured rather than one token, because Sk-VSC colours `%expressions%` and `&a` colour
    // codes *inside* strings and that is most of what a Skript string contains.
    // `prec.right` resolves `""`: without it the parser cannot tell whether the second quote
    // closes this string or opens the next one, and an empty string is common enough to matter.
    // Right associativity means it closes.
    string: $ => prec.right(seq(
      '"',
      repeat(choice(
        $.option_ref_inner,
        $.variable_inner,
        $.interpolation_inner,
        $.color_code,
        $.string_text,
        $.string_marker,
      )),
      // Optional so an unterminated string — which every string is, while it is being typed —
      // highlights as a string instead of turning the rest of the file into an error.
      optional('"'),
    )),
    // `token.immediate` throughout: without it `extras` would eat the spaces inside the string
    // and the text would re-flow.
    //
    // The high precedence is load bearing. tree-sitter resolves lexical conflicts by explicit
    // precedence *before* match length, and a string can legally close after zero pieces, so the
    // keyword tokens below are all candidates at the first character inside a string. Without
    // this, `"tick"` lexed as an empty string, the `time_unit` keyword, and another empty
    // string — every string containing a keyword came apart.
    string_text: () => token.immediate(prec(5, /[^"%&§<{\n]+/)),
    interpolation_inner: () => token.immediate(prec(5, /%[^%\n]*%/)),
    color_code: () => token.immediate(prec(5, choice(/[&§][0-9a-fk-orA-FK-OR]/, /<[a-zA-Z ]+>/))),
    // Sk-VSC colours `{@option}` inside strings differently from the text around it.
    option_ref_inner: () => token.immediate(prec(6, seq('{@', /[^}\n]*/, optional('}')))),
    variable_inner: () => token.immediate(prec(5, seq('{', /[^}\n]*/, optional('}')))),
    // A `%`, `&`, `<` or `{` that did not start a code. Without this the lexer has no way to
    // consume one and the rest of the string becomes an error.
    string_marker: () => token.immediate(prec(4, /[%&§<{]/)),

    // `{@option}` before `{variable}`: both match the same text, so precedence decides.
    option_ref: () => token(prec(2, seq('{@', /[^}\n]*/, optional('}')))),
    variable: () => token(prec(1, seq('{', /[^}\n]*/, optional('}')))),
    interpolation: () => token(/%[^%\n]*%/),

    // Pinned by the trailing colon rather than by column 0 — see the header note.
    event: () => token(prec(3, choice(
      seq(/on [a-zA-Z]/, /[^\n:]*/, ':'),
      seq(/every /, /[^\n:]*/, ':'),
    ))),
    command: () => token(prec(3, seq('command ', /[^\n:]*/, ':'))),
    section: () => token(prec(3, seq(
      choice('options', 'variables', 'aliases', 'import'), /[ \t]*/, ':',
    ))),
    meta_key: () => token(prec(2, seq(
      choice(
        'description', 'usage', 'permission message', 'permission', 'executable by',
        'trigger', 'cooldown message', 'cooldown bypass', 'cooldown storage', 'cooldown',
        'patterns', 'check', 'parse', 'loop of',
      ),
      /[ \t]*/, ':',
    ))),

    boolean_true: () => token(prec(2, 'true')),
    boolean_false: () => token(prec(2, 'false')),

    stop: () => token(prec(2, choice(...STOP))),
    control: () => token(prec(2, choice(...CONTROL))),
    effect: () => token(prec(1, choice(...EFFECT))),
    expression: () => token(prec(1, choice(...EXPRESSION))),
    type: () => token(prec(1, choice(...TYPE))),
    player_object: () => token(prec(1, choice(...PLAYER_OBJECT))),
    connector: () => token(prec(1, choice(...CONNECTOR))),
    time_unit: () => token(prec(1, choice(...TIME_UNIT))),

    // Sk-VSC: keyword.loopobjects.skript
    loop_object: () => token(prec(2, /(loop|event)-[a-zA-Z_][a-zA-Z0-9_]*/)),
    // Sk-VSC: keyword.gui.skript
    gui: () => token(prec(2, choice('gui slot', 'gui', 'slots', 'slot'))),
    // Sk-VSC: keyword.inventory.expression.skript
    inventory: () => token(prec(2, choice(
      'current inventory', 'top inventory', 'open inventory', 'inventories', 'inventory',
    ))),
    // Sk-VSC colours bare colour words outside strings too.
    color_name: () => token(prec(1, choice(
      'black', 'dark grey', 'dark gray', 'light grey', 'light gray', 'grey', 'gray', 'silver',
      'white', 'dark blue', 'light blue', 'blue', 'dark cyan', 'dark aqua', 'cyan', 'aqua',
      'dark green', 'light green', 'lime green', 'lime', 'green', 'light yellow', 'yellow',
      'orange', 'gold', 'dark yellow', 'dark red', 'red', 'pink', 'light red', 'dark purple',
      'light purple', 'purple', 'magenta', 'brown', 'indigo',
    ))),

    // Sk-VSC colours `&a` / `§a` / `<red>` outside strings too — an `options:` block is full of
    // them. Separate from `color_code` because that one is `immediate` and so can only fire
    // inside a string.
    color_code_bare: () => token(prec(3, choice(/[&§][0-9a-fk-orA-FK-OR]/, /<[a-zA-Z ]+>/))),

    number: () => token(/\d+(\.\d+)?/),
    // Sk-VSC: keywork.operator.borders.skript [sic]
    border: () => token(choice('::', ':', '[', ']', '(', ')', '/', '\\')),
    operator: () => token(choice(
      '+', '-', '*', '^', '!=', '=', '>=', '<=', '>', '<', '||', ',', '.', '?', '!', '@', '$',
    )),

    // Everything else. Negative precedence so any keyword of the same length wins, and the
    // excluded characters are the ones that must stay available to the tokens above.
    word: () => token(prec(-1, /[^\s"{}%#:\[\](),]+/)),
  },
});
