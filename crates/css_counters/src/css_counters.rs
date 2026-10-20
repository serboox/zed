use std::collections::HashMap;

/// The name every list item counter goes by.
pub const LIST_ITEM: &str = "list-item";

/// How a counter's value is written down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CounterStyleKind {
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
    LowerGreek,
    /// One glyph whatever the value, as a bullet is.
    Glyph(String),
    /// Nothing at all.
    Hidden,
    Numeric(Vec<String>),
    Alphabetic(Vec<String>),
    Symbolic(Vec<String>),
    Cyclic(Vec<String>),
    Fixed(Vec<String>),
}

/// Writes `value` the way `style` says, falling back to decimal for a value the
/// style has no way to write, as the specification asks.
pub fn format_counter(value: i32, style: &CounterStyleKind) -> String {
    let written = match style {
        CounterStyleKind::Decimal => Some(value.to_string()),
        CounterStyleKind::DecimalLeadingZero => Some(decimal_leading_zero(value)),
        CounterStyleKind::LowerAlpha => alphabetic(value, &LATIN_LOWER),
        CounterStyleKind::UpperAlpha => alphabetic(value, &LATIN_UPPER),
        CounterStyleKind::LowerRoman => roman(value, false),
        CounterStyleKind::UpperRoman => roman(value, true),
        CounterStyleKind::LowerGreek => alphabetic(value, &GREEK_LOWER),
        CounterStyleKind::Glyph(glyph) => Some(glyph.clone()),
        CounterStyleKind::Hidden => Some(String::new()),
        CounterStyleKind::Numeric(symbols) => numeric(value, symbols),
        CounterStyleKind::Alphabetic(symbols) => alphabetic(value, symbols),
        CounterStyleKind::Symbolic(symbols) => symbolic(value, symbols),
        CounterStyleKind::Cyclic(symbols) => cyclic(value, symbols),
        CounterStyleKind::Fixed(symbols) => fixed(value, symbols),
    };
    written.unwrap_or_else(|| value.to_string())
}

const LATIN_LOWER: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];
const LATIN_UPPER: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];
const GREEK_LOWER: [&str; 24] = [
    "α", "β", "γ", "δ", "ε", "ζ", "η", "θ", "ι", "κ", "λ", "μ", "ν", "ξ", "ο", "π", "ρ", "σ", "τ",
    "υ", "φ", "χ", "ψ", "ω",
];

fn decimal_leading_zero(value: i32) -> String {
    if (0..10).contains(&value) {
        format!("0{value}")
    } else if (-9..0).contains(&value) {
        format!("-0{}", -value)
    } else {
        value.to_string()
    }
}

fn roman(value: i32, upper: bool) -> Option<String> {
    if !(1..=3999).contains(&value) {
        return None;
    }
    const NUMERALS: [(i32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut remaining = value;
    let mut written = String::new();
    for (size, numeral) in NUMERALS {
        while remaining >= size {
            written.push_str(numeral);
            remaining -= size;
        }
    }
    Some(if upper {
        written
    } else {
        written.to_lowercase()
    })
}

/// a, b, ... z, aa, ab, ...: no zero, and a value below one cannot be written.
fn alphabetic<S: AsRef<str>>(value: i32, symbols: &[S]) -> Option<String> {
    if value < 1 || symbols.len() < 2 {
        return None;
    }
    let base = symbols.len() as i64;
    let mut remaining = i64::from(value);
    let mut digits = Vec::new();
    while remaining > 0 {
        remaining -= 1;
        digits.push(symbols[(remaining % base) as usize].as_ref());
        remaining /= base;
    }
    Some(digits.iter().rev().copied().collect())
}

/// Positional, with the first symbol standing for zero.
fn numeric<S: AsRef<str>>(value: i32, symbols: &[S]) -> Option<String> {
    if symbols.len() < 2 {
        return None;
    }
    let base = symbols.len() as i64;
    let mut remaining = i64::from(value).abs();
    if remaining == 0 {
        return Some(symbols[0].as_ref().to_string());
    }
    let mut digits = Vec::new();
    while remaining > 0 {
        digits.push(symbols[(remaining % base) as usize].as_ref());
        remaining /= base;
    }
    let written: String = digits.iter().rev().copied().collect();
    Some(if value < 0 {
        format!("-{written}")
    } else {
        written
    })
}

/// The symbol repeated as many times as the value says, then the next symbol
/// that many times, and so on round the list.
fn symbolic<S: AsRef<str>>(value: i32, symbols: &[S]) -> Option<String> {
    if value < 1 || symbols.is_empty() {
        return None;
    }
    let index = (value as usize - 1) % symbols.len();
    let times = (value as usize - 1) / symbols.len() + 1;
    Some(symbols[index].as_ref().repeat(times))
}

fn cyclic<S: AsRef<str>>(value: i32, symbols: &[S]) -> Option<String> {
    if symbols.is_empty() {
        return None;
    }
    let index = (i64::from(value) - 1).rem_euclid(symbols.len() as i64) as usize;
    Some(symbols[index].as_ref().to_string())
}

fn fixed<S: AsRef<str>>(value: i32, symbols: &[S]) -> Option<String> {
    if value < 1 {
        return None;
    }
    symbols
        .get(value as usize - 1)
        .map(|s| s.as_ref().to_string())
}

/// The value of every counter in scope at one place in a document, each with the
/// instances that nest, outermost first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CounterValues(Vec<(String, Vec<i32>)>);

impl CounterValues {
    /// The innermost instance of `name`, or zero where there is none, which is the
    /// value a counter nobody has reset has.
    pub fn innermost(&self, name: &str) -> i32 {
        self.all(name).last().copied().unwrap_or(0)
    }

    /// Every instance of `name` in scope, outermost first.
    pub fn all(&self, name: &str) -> &[i32] {
        self.0
            .iter()
            .find(|(counter, _)| counter == name)
            .map(|(_, values)| values.as_slice())
            .unwrap_or(&[])
    }
}

#[derive(Debug)]
struct Instance {
    value: i32,
    /// How deep in the document the element that made this counter sits.
    frame: usize,
}

/// Counters as a walk through a document in tree order sees them. An element is
/// walked with `reset`, `increment` and `set`, then its children between
/// `open_children` and `close_children`.
///
/// A counter made by an element lives in the element's following siblings and
/// their descendants as well as in its own, and goes away when their parent
/// does. Two siblings that both reset a name do not nest: the later one takes
/// over the earlier one's instance.
#[derive(Debug)]
pub struct CounterScopes {
    stacks: HashMap<String, Vec<Instance>>,
    /// For each open parent, the names it has made an instance of.
    frames: Vec<Vec<String>>,
}

impl Default for CounterScopes {
    fn default() -> Self {
        Self::new()
    }
}

impl CounterScopes {
    pub fn new() -> Self {
        Self {
            stacks: HashMap::new(),
            frames: vec![Vec::new()],
        }
    }

    pub fn open_children(&mut self) {
        self.frames.push(Vec::new());
    }

    pub fn close_children(&mut self) {
        if self.frames.len() <= 1 {
            return;
        }
        for name in self.frames.pop().unwrap_or_default() {
            if let Some(stack) = self.stacks.get_mut(&name) {
                stack.pop();
            }
        }
    }

    pub fn reset(&mut self, name: &str, value: i32) {
        let frame = self.frames.len() - 1;
        let stack = self.stacks.entry(name.to_string()).or_default();
        match stack.last_mut() {
            Some(top) if top.frame == frame => top.value = value,
            _ => {
                stack.push(Instance { value, frame });
                self.frames[frame].push(name.to_string());
            }
        }
    }

    pub fn increment(&mut self, name: &str, by: i32) {
        self.ensure(name);
        if let Some(top) = self.stacks.get_mut(name).and_then(|stack| stack.last_mut()) {
            top.value = top.value.saturating_add(by);
        }
    }

    pub fn set(&mut self, name: &str, value: i32) {
        self.ensure(name);
        if let Some(top) = self.stacks.get_mut(name).and_then(|stack| stack.last_mut()) {
            top.value = value;
        }
    }

    fn ensure(&mut self, name: &str) {
        if self.stacks.get(name).is_none_or(|stack| stack.is_empty()) {
            self.reset(name, 0);
        }
    }

    pub fn snapshot(&self) -> CounterValues {
        CounterValues(
            self.stacks
                .iter()
                .filter(|(_, stack)| !stack.is_empty())
                .map(|(name, stack)| {
                    (
                        name.clone(),
                        stack.iter().map(|instance| instance.value).collect(),
                    )
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(value: i32, style: CounterStyleKind) -> String {
        format_counter(value, &style)
    }

    #[test]
    fn decimal_and_its_leading_zero_variant() {
        assert_eq!(written(7, CounterStyleKind::Decimal), "7");
        assert_eq!(written(-3, CounterStyleKind::Decimal), "-3");
        assert_eq!(written(7, CounterStyleKind::DecimalLeadingZero), "07");
        assert_eq!(written(10, CounterStyleKind::DecimalLeadingZero), "10");
        assert_eq!(written(-4, CounterStyleKind::DecimalLeadingZero), "-04");
    }

    #[test]
    fn letters_go_on_past_z_and_never_reach_zero() {
        assert_eq!(written(1, CounterStyleKind::LowerAlpha), "a");
        assert_eq!(written(26, CounterStyleKind::LowerAlpha), "z");
        assert_eq!(written(27, CounterStyleKind::LowerAlpha), "aa");
        assert_eq!(written(52, CounterStyleKind::UpperAlpha), "AZ");
        assert_eq!(written(0, CounterStyleKind::LowerAlpha), "0");
        assert_eq!(written(2, CounterStyleKind::LowerGreek), "β");
        assert_eq!(written(25, CounterStyleKind::LowerGreek), "αα");
    }

    #[test]
    fn roman_numerals_within_their_range() {
        assert_eq!(written(4, CounterStyleKind::LowerRoman), "iv");
        assert_eq!(written(1994, CounterStyleKind::UpperRoman), "MCMXCIV");
        assert_eq!(written(3999, CounterStyleKind::UpperRoman), "MMMCMXCIX");
        assert_eq!(written(4000, CounterStyleKind::UpperRoman), "4000");
        assert_eq!(written(0, CounterStyleKind::LowerRoman), "0");
    }

    #[test]
    fn a_bullet_is_the_same_whatever_the_value() {
        let bullet = CounterStyleKind::Glyph("•".to_string());
        assert_eq!(written(1, bullet.clone()), "•");
        assert_eq!(written(99, bullet), "•");
        assert_eq!(written(5, CounterStyleKind::Hidden), "");
    }

    #[test]
    fn styles_built_from_symbols() {
        let symbols: Vec<String> = ["0", "1"].iter().map(|s| s.to_string()).collect();
        assert_eq!(written(5, CounterStyleKind::Numeric(symbols)), "101");
        let abc: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(written(4, CounterStyleKind::Alphabetic(abc.clone())), "aa");
        assert_eq!(written(5, CounterStyleKind::Symbolic(abc.clone())), "bb");
        assert_eq!(written(4, CounterStyleKind::Cyclic(abc.clone())), "a");
        assert_eq!(written(0, CounterStyleKind::Cyclic(abc.clone())), "c");
        assert_eq!(written(3, CounterStyleKind::Fixed(abc.clone())), "c");
        assert_eq!(written(4, CounterStyleKind::Fixed(abc)), "4");
    }

    #[test]
    fn an_unreset_counter_starts_at_zero_when_it_is_first_incremented() {
        let mut scopes = CounterScopes::new();
        scopes.increment("n", 1);
        scopes.increment("n", 1);
        assert_eq!(scopes.snapshot().innermost("n"), 2);
        assert_eq!(scopes.snapshot().innermost("missing"), 0);
    }

    #[test]
    fn a_counter_reaches_the_siblings_after_its_element_and_their_children() {
        let mut scopes = CounterScopes::new();
        // <body> reset c; <h2> increment c, <p> reads c, inside it <b> reads c
        scopes.open_children();
        scopes.reset("c", 0);
        scopes.increment("c", 1);
        scopes.open_children();
        assert_eq!(scopes.snapshot().innermost("c"), 1);
        scopes.close_children();
        scopes.increment("c", 1);
        assert_eq!(scopes.snapshot().innermost("c"), 2);
        scopes.close_children();
        assert_eq!(scopes.snapshot().innermost("c"), 0);
    }

    #[test]
    fn a_nested_reset_makes_an_instance_that_goes_when_its_parent_does() {
        let mut scopes = CounterScopes::new();
        scopes.open_children();
        scopes.reset("item", 0);
        scopes.increment("item", 1);
        scopes.open_children();
        scopes.reset("item", 0);
        scopes.increment("item", 1);
        scopes.increment("item", 1);
        assert_eq!(scopes.snapshot().all("item"), &[1, 2]);
        scopes.close_children();
        assert_eq!(scopes.snapshot().all("item"), &[1]);
        scopes.increment("item", 1);
        assert_eq!(scopes.snapshot().innermost("item"), 2);
    }

    #[test]
    fn two_siblings_that_reset_a_name_do_not_nest() {
        let mut scopes = CounterScopes::new();
        scopes.open_children();
        scopes.reset("x", 5);
        scopes.reset("x", 9);
        assert_eq!(scopes.snapshot().all("x"), &[9]);
    }

    #[test]
    fn set_creates_and_overwrites() {
        let mut scopes = CounterScopes::new();
        scopes.set("n", 7);
        assert_eq!(scopes.snapshot().innermost("n"), 7);
        scopes.increment("n", 1);
        scopes.set("n", 3);
        assert_eq!(scopes.snapshot().innermost("n"), 3);
    }
}
