//! Bounded Unicode layout for a deliberately small TeX math subset.
//! Accents apply only to single graphemes so their scope survives terminal rendering.
//! Unsupported Unicode scripts use raised/lowered grouping; structured layouts remain bounded.

use crate::width::display_width;
use unicode_segmentation::UnicodeSegmentation;

mod structured;

const MAX_ROWS: usize = 16;
const MAX_COLUMNS: usize = 256;

struct Layout {
    rows: Vec<String>,
    baseline: usize,
}

impl Layout {
    fn text(text: impl Into<String>) -> Self {
        Self {
            rows: vec![text.into()],
            baseline: 0,
        }
    }

    fn width(&self) -> usize {
        self.rows
            .iter()
            .map(|row| display_width(row))
            .max()
            .unwrap_or_default()
    }

    fn join(self, right: Self) -> Option<Self> {
        let baseline = self.baseline.max(right.baseline);
        let height = (baseline + self.rows.len() - self.baseline)
            .max(baseline + right.rows.len() - right.baseline);
        // Separate neighboring fraction bars so their numerators cannot become one number.
        let width = self.width() + usize::from(self.rows.len() > 1 && right.rows.len() > 1);
        if height > MAX_ROWS || width + right.width() > MAX_COLUMNS {
            return None;
        }
        let mut rows = vec![String::new(); height];
        for (index, row) in rows.iter_mut().enumerate() {
            if let Some(left) = index
                .checked_sub(baseline - self.baseline)
                .and_then(|i| self.rows.get(i))
            {
                row.push_str(left);
            }
            row.push_str(&" ".repeat(width - display_width(row)));
            if let Some(right) = index
                .checked_sub(baseline - right.baseline)
                .and_then(|i| right.rows.get(i))
            {
                row.push_str(right);
            }
        }
        Some(Self { rows, baseline })
    }

    fn single(&self) -> Option<&str> {
        (self.rows.len() == 1).then(|| self.rows[0].as_str())
    }
}

pub(super) fn render(source: &str, display: bool) -> Option<String> {
    let mut parser = MathParser {
        remaining: source.trim(),
        depth: 0,
        display,
        stack_annotations: display,
    };
    let result = parser.sequence(SequenceEnd::Input)?;
    (!result.rows.iter().all(|row| row.trim().is_empty())).then(|| result.rows.join("\n"))
}

struct MathParser<'a> {
    remaining: &'a str,
    depth: usize,
    display: bool,
    stack_annotations: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum SequenceEnd {
    Input,
    Group,
    Cell,
}

#[derive(Clone, Copy)]
enum ScriptPosition {
    Above,
    Below,
}

impl MathParser<'_> {
    fn take(&mut self) -> Option<char> {
        let ch = self.remaining.chars().next()?;
        self.remaining = &self.remaining[ch.len_utf8()..];
        Some(ch)
    }

    fn sequence(&mut self, end: SequenceEnd) -> Option<Layout> {
        if self.depth >= 32 {
            return None;
        }
        self.depth += 1;
        let mut result = Layout::text("");
        let mut scripts = 0;
        let mut has_base = false;
        while let Some(ch) = self.remaining.chars().next() {
            let remaining = self.remaining.trim_start();
            if end == SequenceEnd::Cell
                && (remaining.starts_with(['&', '}'])
                    || remaining.starts_with(r"\\")
                    || remaining.starts_with(r"\end{"))
            {
                self.remaining = remaining;
                self.depth -= 1;
                return Some(result);
            }
            if ch == '}' {
                if end != SequenceEnd::Group {
                    return None;
                }
                self.take();
                self.depth -= 1;
                return Some(result);
            }
            if ch == '^' || ch == '_' {
                let script = if ch == '^' { 1 } else { 2 };
                if !has_base || scripts & script != 0 {
                    return None;
                }
                scripts |= script;
            } else if !ch.is_whitespace() {
                scripts = 0;
            }
            let atom = self.atom()?;
            if !ch.is_whitespace() && ch != '^' && ch != '_' {
                // Flattening a compound base would change the scope of a following script.
                has_base = atom
                    .single()
                    .is_some_and(|text| text.graphemes(/*is_extended*/ true).count() == 1);
            }
            result = result.join(atom)?;
        }
        self.depth -= 1;
        (end == SequenceEnd::Input).then_some(result)
    }

    fn argument(&mut self) -> Option<Layout> {
        self.remaining = self.remaining.trim_start();
        if self.remaining.starts_with(['^', '_']) {
            return None;
        }
        self.atom()
    }

    fn atom(&mut self) -> Option<Layout> {
        if self.depth >= 32 {
            return None;
        }
        self.depth += 1;
        let result = self.atom_inner();
        self.depth -= 1;
        result
    }

    fn atom_inner(&mut self) -> Option<Layout> {
        let ch = self.take()?;
        match ch {
            '{' => self.sequence(SequenceEnd::Group),
            '\\' => self.command(),
            '^' | '_' => {
                let arg = self.compact_argument()?;
                let text = arg.single()?;
                let position = if ch == '^' {
                    ScriptPosition::Above
                } else {
                    ScriptPosition::Below
                };
                Some(Layout::text(render_script(text, position)))
            }
            '}' | '$' | '%' | '#' | '&' | '`' => None,
            ch if ch.is_whitespace() => {
                self.remaining = self.remaining.trim_start();
                Some(Layout::text(" "))
            }
            ch if ch.is_control() => None,
            ch => Some(Layout::text(ch.to_string())),
        }
    }

    fn command(&mut self) -> Option<Layout> {
        let length = self
            .remaining
            .bytes()
            .take_while(u8::is_ascii_alphabetic)
            .count();
        if length == 0 {
            return match self.take()? {
                ',' | ';' | ':' | ' ' => Some(Layout::text(" ")),
                '!' => Some(Layout::text("")),
                '{' => Some(Layout::text("{")),
                '}' => Some(Layout::text("}")),
                '|' => Some(Layout::text("‖")),
                _ => None,
            };
        }
        let name = &self.remaining[..length];
        self.remaining = &self.remaining[length..];
        match name {
            "sum" | "bigwedge" if self.stack_annotations => {
                self.display_operator(if name == "sum" { "∑" } else { "⋀" })
            }
            "begin" | "boxed" | "underset" | "overset" | "substack" | "mathcal" => {
                self.structured_command(name)
            }
            "frac" | "dfrac" | "tfrac" => {
                let numerator = self.compact_argument()?;
                let denominator = self.compact_argument()?;
                if !self.display {
                    return Some(Layout::text(format!(
                        "(({})/({}))",
                        numerator.single()?,
                        denominator.single()?
                    )));
                }
                // Nested bars need a richer layout to preserve fraction hierarchy.
                let numerator = numerator.single()?;
                let denominator = denominator.single()?;
                let width = display_width(numerator)
                    .max(display_width(denominator))
                    .max(/*other*/ 1);
                if width > MAX_COLUMNS {
                    return None;
                }
                Some(Layout {
                    rows: vec![
                        format!(
                            "{}{numerator}",
                            " ".repeat((width - display_width(numerator)) / 2)
                        ),
                        "─".repeat(width),
                        format!(
                            "{}{denominator}",
                            " ".repeat((width - display_width(denominator)) / 2)
                        ),
                    ],
                    baseline: 1,
                })
            }
            "sqrt" => {
                if self.remaining.trim_start().starts_with('[') {
                    return None;
                }
                let radicand = self.compact_argument()?;
                Some(Layout::text(format!("√({})", radicand.single()?)))
            }
            "mathbb" => {
                let arg = self.argument()?;
                let text = match arg.single()? {
                    "R" => "ℝ",
                    "C" => "ℂ",
                    "N" => "ℕ",
                    "Z" => "ℤ",
                    "Q" => "ℚ",
                    "P" => "ℙ",
                    _ => return None,
                };
                Some(Layout::text(text))
            }
            "hat" | "widehat" | "bar" | "overline" | "tilde" | "widetilde" | "vec"
            | "overrightarrow" | "dot" | "ddot" | "check" | "breve" | "acute" | "grave" => {
                let arg = self.argument()?;
                let text = arg.single()?;
                if text.graphemes(/*is_extended*/ true).count() != 1
                    || text.trim().is_empty()
                    || display_width(text) == 0
                {
                    return None;
                }
                let accent = match name {
                    "hat" | "widehat" => '\u{0302}',
                    "bar" | "overline" => '\u{0304}',
                    "tilde" | "widetilde" => '\u{0303}',
                    "vec" | "overrightarrow" => '\u{20d7}',
                    "dot" => '\u{0307}',
                    "ddot" => '\u{0308}',
                    "check" => '\u{030c}',
                    "breve" => '\u{0306}',
                    "acute" => '\u{0301}',
                    "grave" => '\u{0300}',
                    _ => unreachable!(),
                };
                Some(Layout::text(format!("{text}{accent}")))
            }
            "mathrm" | "mathbf" | "mathit" => self.argument(),
            "text" | "operatorname" => {
                self.remaining = self.remaining.trim_start().strip_prefix('{')?;
                let end = self.remaining.find('}')?;
                let text = &self.remaining[..end];
                if text.contains(['{', '\\', '$', '%', '#', '&'])
                    || text.chars().any(char::is_control)
                {
                    return None;
                }
                self.remaining = &self.remaining[end + 1..];
                Some(Layout::text(text))
            }
            "left" | "right" | "bigl" | "bigr" => {
                self.remaining = self.remaining.trim_start();
                match self.take()? {
                    '.' => Some(Layout::text("")),
                    ch @ ('(' | ')' | '[' | ']' | '|') => Some(Layout::text(ch.to_string())),
                    '<' => Some(Layout::text("⟨")),
                    '>' => Some(Layout::text("⟩")),
                    '\\' => {
                        let length = self
                            .remaining
                            .bytes()
                            .take_while(u8::is_ascii_alphabetic)
                            .count()
                            .max(/*other*/ 1);
                        let (name, remaining) = self.remaining.split_at_checked(length)?;
                        self.remaining = remaining;
                        delimiter(name).map(Layout::text)
                    }
                    _ => None,
                }
            }
            "quad" | "qquad" => Some(Layout::text(" ")),
            "sin" | "cos" | "tan" | "log" | "ln" | "exp" | "lim" | "max" | "min" => {
                Some(Layout::text(name))
            }
            _ => symbol(name).map(Layout::text),
        }
    }
}

fn render_script(text: &str, position: ScriptPosition) -> String {
    let converted = text
        .chars()
        .map(|ch| script_char(ch, position))
        .collect::<Option<String>>();
    converted.unwrap_or_else(|| {
        let (open, close) = match position {
            ScriptPosition::Above => ('⁽', '⁾'),
            ScriptPosition::Below => ('₍', '₎'),
        };
        format!("{open}{text}{close}")
    })
}

fn script_char(ch: char, position: ScriptPosition) -> Option<char> {
    // Unicode does not define a complete superscript/subscript alphabet. Only use exact
    // codepoints here: visually substituting a lowercase modifier for an unsupported capital
    // would change mathematical notation. The caller keeps those labels explicit and grouped.
    match position {
        ScriptPosition::Above => Some(match ch {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '+' => '⁺',
            '-' => '⁻',
            '=' => '⁼',
            '(' => '⁽',
            ')' => '⁾',
            '*' => '⁎',
            'a' => 'ᵃ',
            'b' => 'ᵇ',
            'c' => 'ᶜ',
            'd' => 'ᵈ',
            'e' => 'ᵉ',
            'f' => 'ᶠ',
            'g' => 'ᵍ',
            'h' => 'ʰ',
            'i' => 'ⁱ',
            'j' => 'ʲ',
            'k' => 'ᵏ',
            'l' => 'ˡ',
            'm' => 'ᵐ',
            'n' => 'ⁿ',
            'o' => 'ᵒ',
            'p' => 'ᵖ',
            'r' => 'ʳ',
            's' => 'ˢ',
            't' => 'ᵗ',
            'u' => 'ᵘ',
            'v' => 'ᵛ',
            'w' => 'ʷ',
            'x' => 'ˣ',
            'y' => 'ʸ',
            'z' => 'ᶻ',
            'A' => 'ᴬ',
            'B' => 'ᴮ',
            'C' => 'ꟲ',
            'D' => 'ᴰ',
            'E' => 'ᴱ',
            'F' => 'ꟳ',
            'G' => 'ᴳ',
            'H' => 'ᴴ',
            'I' => 'ᴵ',
            'J' => 'ᴶ',
            'K' => 'ᴷ',
            'L' => 'ᴸ',
            'M' => 'ᴹ',
            'N' => 'ᴺ',
            'O' => 'ᴼ',
            'P' => 'ᴾ',
            'Q' => 'ꟴ',
            'R' => 'ᴿ',
            'T' => 'ᵀ',
            'U' => 'ᵁ',
            'V' => 'ⱽ',
            'W' => 'ᵂ',
            'α' => 'ᵅ',
            'β' => 'ᵝ',
            'γ' => 'ᵞ',
            'δ' => 'ᵟ',
            'θ' => 'ᶿ',
            'ι' => 'ᶥ',
            'φ' => 'ᶲ',
            'ϕ' => 'ᵠ',
            'χ' => 'ᵡ',
            // Prime glyphs already carry their own vertical placement.
            '′' | '″' | '‴' | '⁗' => ch,
            // Unicode has no general script-height punctuation. A narrow separator remains
            // unambiguous between raised/lowered atoms, as in `a_{j,t}`.
            ',' | ';' | ':' => ch,
            _ => return None,
        }),
        ScriptPosition::Below => Some(match ch {
            '0' => '₀',
            '1' => '₁',
            '2' => '₂',
            '3' => '₃',
            '4' => '₄',
            '5' => '₅',
            '6' => '₆',
            '7' => '₇',
            '8' => '₈',
            '9' => '₉',
            '+' => '₊',
            '-' => '₋',
            '=' => '₌',
            '(' => '₍',
            ')' => '₎',
            'a' => 'ₐ',
            'e' => 'ₑ',
            'h' => 'ₕ',
            'i' => 'ᵢ',
            'j' => 'ⱼ',
            'k' => 'ₖ',
            'l' => 'ₗ',
            'm' => 'ₘ',
            'n' => 'ₙ',
            'o' => 'ₒ',
            'p' => 'ₚ',
            'r' => 'ᵣ',
            's' => 'ₛ',
            't' => 'ₜ',
            'u' => 'ᵤ',
            'v' => 'ᵥ',
            'x' => 'ₓ',
            'β' => 'ᵦ',
            'γ' => 'ᵧ',
            'ρ' => 'ᵨ',
            'φ' | 'ϕ' => 'ᵩ',
            'χ' => 'ᵪ',
            ',' | ';' | ':' => ch,
            _ => return None,
        }),
    }
}

fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "varkappa" => "ϰ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "aleph" => "ℵ",
        "imath" => "ı",
        "jmath" => "ȷ",
        "prime" => "′",
        "angle" => "∠",
        "dagger" => "†",
        "ddagger" => "‡",
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "cdot" => "·",
        "div" => "÷",
        "circ" => "∘",
        "bullet" => "∙",
        "oplus" => "⊕",
        "otimes" => "⊗",
        "odot" => "⊙",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "propto" => "∝",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "lesssim" => "≲",
        "gtrsim" => "≳",
        "ll" => "≪",
        "gg" => "≫",
        "perp" => "⊥",
        "parallel" => "∥",
        "equiv" => "≡",
        "in" => "∈",
        "notin" => "∉", // codespell:ignore notin
        "ni" => "∋",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "bigcup" => "⋃",
        "bigcap" => "⋂",
        "bigwedge" => "⋀",
        "setminus" => "∖",
        "emptyset" | "varnothing" => "∅",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "neg" | "lnot" => "¬",
        "top" => "⊤",
        "bot" => "⊥",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "to" | "rightarrow" => "→",
        "leftarrow" => "←",
        "leftrightarrow" => "↔",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "updownarrow" => "↕",
        "Leftarrow" | "impliedby" => "⇐",
        "Rightarrow" | "implies" => "⇒",
        "Leftrightarrow" | "iff" => "⇔",
        "ldots" | "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        _ => return delimiter(name),
    })
}

fn delimiter(name: &str) -> Option<&'static str> {
    Some(match name {
        "langle" => "⟨",
        "rangle" => "⟩",
        "lbrace" | "{" => "{",
        "rbrace" | "}" => "}",
        "lbrack" => "[",
        "rbrack" => "]",
        "vert" | "lvert" | "rvert" => "|",
        "Vert" | "lVert" | "rVert" | "|" => "‖",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        _ => return None,
    })
}
