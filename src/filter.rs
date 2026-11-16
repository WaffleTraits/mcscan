//! The `--filter` expression language.

use crate::item::{norm_id, short_id, ItemView};
use crate::nbt::{self, Seg, Tag};
use anyhow::{anyhow, bail, Result};
use regex::Regex;
use std::borrow::Cow;
use std::cmp::Ordering;

pub const HELP: &str = r#"FILTER EXPRESSIONS  (scan --filter / -f; several -f are ANDed together)

  id == diamond_sword && ench(sharpness) > 5
  id like *_shulker_box and depth > 0
  count > 64 or has(unbreakable)
  id in (bedrock, barrier, command_block) and source == player
  name ~ "(?i)dupe" || nbt(components.minecraft:custom_data.owner) == Steve

Fields
  id          item id; "minecraft:" is optional everywhere
  count       stack size
  slot        inventory slot, if known
  damage      durability used, if any
  name        custom (anvil) name as plain text, "" if unnamed
  depth       0 = directly in an inventory, 1 = inside a shulker/bundle there, ...
  holder      id of the block entity / entity holding it (chest, item_frame, ...) or "player"
  source      block_entity | entity | player | level | data
  dim         dimension id (overworld, the_nether, mypack:mine, ...)
  x y z       position of the holder
  uuid        player UUID, for items found in player data
  file        world-relative file the item is stored in
  max_ench    highest enchantment level on the item (0 if none)
  ench_count  number of enchantments on the item
  snbt        the whole item as SNBT text, handy with ~

Functions
  ench(NAME)  level of an enchantment (regular or stored in a book), 0 if absent
  has(COMP)   true if the item has the component, e.g. has(unbreakable), has(custom_data)
  comp(COMP)  value of a component: comp(damage), comp(max_stack_size)
  nbt(PATH)   value at an NBT path inside the item, e.g.
              nbt(components.minecraft:fireworks.flight_duration)
              nbt(tag.display.Name)          (pre-1.20.5 item format)

Operators
  == !=  < <= > >=        compare numbers or text ("minecraft:" is ignored)
  like                    glob with * and ?        id like *_spawn_egg
  ~  !~                   regular expression       snbt ~ "lvl:3276[0-9]s"
  contains                substring                name contains admin
  in (a, b, c)            any of
  and or not  && || !     boolean logic, with ( ) for grouping

A bare value such as  has(lore)  or  slot  is true when it is present, non-zero
and non-empty. Words on the right-hand side of an operator are plain text, so
quotes are only needed for values containing spaces or symbols."#;

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

/// `*` matches any run of characters, `?` exactly one.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Glob on ids, ignoring an optional `minecraft:` on either side.
pub fn id_glob(pattern: &str, id: &str) -> bool {
    glob_match(short_id(pattern), short_id(id))
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Match,
    NotMatch,
    Like,
    Contains,
    In,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Word(String),
    LParen,
    RParen,
    Comma,
    Op(Op),
    And,
    Or,
    Not,
}

/// Characters (besides letters and digits) that may continue an unquoted word:
/// enough for ids, globs, UUIDs and NBT paths.
const WORD_CHARS: &str = "_:.*?/#-[]\"";

fn lex(src: &str) -> Result<Vec<Tok>> {
    let c: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let two = |i: usize, a: char, b: char| c.get(i) == Some(&a) && c.get(i + 1) == Some(&b);
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '(' {
            out.push(Tok::LParen);
            i += 1;
        } else if ch == ')' {
            out.push(Tok::RParen);
            i += 1;
        } else if ch == ',' {
            out.push(Tok::Comma);
            i += 1;
        } else if two(i, '&', '&') {
            out.push(Tok::And);
            i += 2;
        } else if two(i, '|', '|') {
            out.push(Tok::Or);
            i += 2;
        } else if two(i, '=', '=') {
            out.push(Tok::Op(Op::Eq));
            i += 2;
        } else if two(i, '!', '=') {
            out.push(Tok::Op(Op::Ne));
            i += 2;
        } else if two(i, '!', '~') {
            out.push(Tok::Op(Op::NotMatch));
            i += 2;
        } else if two(i, '<', '=') {
            out.push(Tok::Op(Op::Le));
            i += 2;
        } else if two(i, '>', '=') {
            out.push(Tok::Op(Op::Ge));
            i += 2;
        } else if ch == '=' {
            out.push(Tok::Op(Op::Eq));
            i += 1;
        } else if ch == '<' {
            out.push(Tok::Op(Op::Lt));
            i += 1;
        } else if ch == '>' {
            out.push(Tok::Op(Op::Gt));
            i += 1;
        } else if ch == '~' {
            out.push(Tok::Op(Op::Match));
            i += 1;
        } else if ch == '!' {
            out.push(Tok::Not);
            i += 1;
        } else if ch == '"' || ch == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                match c.get(i) {
                    None => bail!("unterminated string in filter"),
                    Some('\\') if c.get(i + 1) == Some(&ch) => {
                        s.push(ch);
                        i += 2;
                    }
                    Some(x) if *x == ch => {
                        i += 1;
                        break;
                    }
                    Some(x) => {
                        s.push(*x);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if ch.is_ascii_digit() || (ch == '-' && c.get(i + 1).map_or(false, |n| n.is_ascii_digit())) {
            let start = i;
            i += 1;
            while i < c.len() && (c[i].is_alphanumeric() || WORD_CHARS.contains(c[i])) {
                i += 1;
            }
            let text: String = c[start..i].iter().collect();
            // Things like "1111*", "1.21-pre" or a UUID are text, not numbers.
            match text.parse::<f64>() {
                Ok(n) => out.push(Tok::Num(n)),
                Err(_) => out.push(Tok::Word(text)),
            }
        } else if ch.is_alphanumeric() || "_#*?.".contains(ch) {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || WORD_CHARS.contains(c[i])) {
                i += 1;
            }
            let text: String = c[start..i].iter().collect();
            out.push(match text.to_ascii_lowercase().as_str() {
                "and" => Tok::And,
                "or" => Tok::Or,
                "not" => Tok::Not,
                "like" => Tok::Op(Op::Like),
                "contains" => Tok::Op(Op::Contains),
                "in" => Tok::Op(Op::In),
                "matches" => Tok::Op(Op::Match),
                _ => Tok::Word(text),
            });
        } else {
            bail!("unexpected character {ch:?} in filter");
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Field {
    Id,
    Count,
    Slot,
    Damage,
    Name,
    Depth,
    Holder,
    Source,
    Dim,
    X,
    Y,
    Z,
    Uuid,
    File,
    MaxEnch,
    EnchCount,
    Snbt,
}

fn field(name: &str) -> Option<Field> {
    Some(match name.to_ascii_lowercase().as_str() {
        "id" => Field::Id,
        "count" => Field::Count,
        "slot" => Field::Slot,
        "damage" => Field::Damage,
        "name" => Field::Name,
        "depth" => Field::Depth,
        "holder" => Field::Holder,
        "source" => Field::Source,
        "dim" | "dimension" => Field::Dim,
        "x" => Field::X,
        "y" => Field::Y,
        "z" => Field::Z,
        "uuid" => Field::Uuid,
        "file" => Field::File,
        "max_ench" => Field::MaxEnch,
        "ench_count" => Field::EnchCount,
        "snbt" => Field::Snbt,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
enum Val {
    Num(f64),
    Str(String),
    Bool(bool),
    Field(Field),
    Ench(String),
    Has(String),
    Comp(String),
    Nbt(Vec<Seg>),
}

#[derive(Debug, Clone)]
enum Node {
    Or(Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Not(Box<Node>),
    Cmp(Val, Op, Val),
    Regex(Val, Regex, bool),
    In(Val, Vec<Val>),
    Truthy(Val),
}

/// A compiled filter expression.
#[derive(Debug, Clone)]
pub struct Filter {
    root: Node,
    pub source: String,
}

struct Parser {
    t: Vec<Tok>,
    p: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.p)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.t.get(self.p).cloned();
        self.p += 1;
        t
    }
    fn expect(&mut self, want: Tok, what: &str) -> Result<()> {
        match self.next() {
            Some(t) if t == want => Ok(()),
            other => bail!("expected {what}, found {}", describe(other.as_ref())),
        }
    }

    fn or(&mut self) -> Result<Node> {
        let mut left = self.and()?;
        while self.peek() == Some(&Tok::Or) {
            self.p += 1;
            left = Node::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Node> {
        let mut left = self.not()?;
        while self.peek() == Some(&Tok::And) {
            self.p += 1;
            left = Node::And(Box::new(left), Box::new(self.not()?));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Node> {
        if self.peek() == Some(&Tok::Not) {
            self.p += 1;
            return Ok(Node::Not(Box::new(self.not()?)));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Node> {
        if self.peek() == Some(&Tok::LParen) {
            self.p += 1;
            let inner = self.or()?;
            self.expect(Tok::RParen, "')'")?;
            return Ok(inner);
        }
        let left = self.val(true)?;
        let op = match self.peek() {
            Some(Tok::Op(op)) => *op,
            _ => return Ok(Node::Truthy(left)),
        };
        self.p += 1;
        if op == Op::In {
            self.expect(Tok::LParen, "'(' after 'in'")?;
            let mut items = Vec::new();
            loop {
                items.push(self.val(false)?);
                match self.next() {
                    Some(Tok::Comma) => {}
                    Some(Tok::RParen) => break,
                    other => bail!("expected ',' or ')' in list, found {}", describe(other.as_ref())),
                }
            }
            return Ok(Node::In(left, items));
        }
        let right = self.val(false)?;
        if matches!(op, Op::Match | Op::NotMatch) {
            let pattern = match &right {
                Val::Str(s) => s.clone(),
                Val::Num(n) => n.to_string(),
                _ => bail!("the right-hand side of ~ must be a regular expression"),
            };
            let re = Regex::new(&pattern).map_err(|e| anyhow!("bad regular expression {pattern:?}: {e}"))?;
            return Ok(Node::Regex(left, re, op == Op::NotMatch));
        }
        Ok(Node::Cmp(left, op, right))
    }

    /// `lhs`: bare words name fields. On the right-hand side they are text.
    fn val(&mut self, lhs: bool) -> Result<Val> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(Val::Num(n)),
            Some(Tok::Str(s)) => Ok(Val::Str(s)),
            Some(Tok::Word(w)) => {
                if self.peek() == Some(&Tok::LParen) {
                    self.p += 1;
                    let arg = match self.next() {
                        Some(Tok::Word(a)) | Some(Tok::Str(a)) => a,
                        other => bail!("{w}() needs one name argument, found {}", describe(other.as_ref())),
                    };
                    self.expect(Tok::RParen, "')'")?;
                    return Ok(match w.to_ascii_lowercase().as_str() {
                        "ench" => Val::Ench(norm_id(&arg).into_owned()),
                        "has" => Val::Has(component_name(&arg)),
                        "comp" => Val::Comp(component_name(&arg)),
                        "nbt" => Val::Nbt(nbt::parse_path(&arg)?),
                        _ => bail!("unknown function {w}() - available: ench, has, comp, nbt"),
                    });
                }
                match w.to_ascii_lowercase().as_str() {
                    "true" => return Ok(Val::Bool(true)),
                    "false" => return Ok(Val::Bool(false)),
                    _ => {}
                }
                if lhs {
                    match field(&w) {
                        Some(f) => Ok(Val::Field(f)),
                        None => bail!("unknown field {w:?} - run `mcscan fields` for the list"),
                    }
                } else {
                    Ok(Val::Str(w))
                }
            }
            other => bail!("expected a value, found {}", describe(other.as_ref())),
        }
    }
}

fn component_name(arg: &str) -> String {
    match arg.strip_prefix('!') {
        Some(rest) => format!("!{}", norm_id(rest)),
        None => norm_id(arg).into_owned(),
    }
}

fn describe(t: Option<&Tok>) -> String {
    match t {
        None => "end of expression".to_string(),
        Some(Tok::Num(n)) => n.to_string(),
        Some(Tok::Str(s)) => format!("{s:?}"),
        Some(Tok::Word(w)) => w.clone(),
        Some(Tok::LParen) => "'('".into(),
        Some(Tok::RParen) => "')'".into(),
        Some(Tok::Comma) => "','".into(),
        Some(Tok::Op(_)) => "an operator".into(),
        Some(Tok::And) => "'and'".into(),
        Some(Tok::Or) => "'or'".into(),
        Some(Tok::Not) => "'not'".into(),
    }
}

impl Filter {
    pub fn parse(src: &str) -> Result<Filter> {
        let mut p = Parser { t: lex(src)?, p: 0 };
        if p.t.is_empty() {
            bail!("empty filter expression");
        }
        let root = p.or().map_err(|e| anyhow!("in filter {src:?}: {e}"))?;
        if p.p < p.t.len() {
            bail!("in filter {src:?}: unexpected {}", describe(p.peek()));
        }
        Ok(Filter { root, source: src.to_string() })
    }

    pub fn matches(&self, cx: &EvalCtx) -> bool {
        eval(&self.root, cx)
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Everything a filter can ask about one item.
pub struct EvalCtx<'a> {
    pub view: &'a ItemView<'a>,
    pub holder: Option<&'a str>,
    pub source: &'a str,
    pub dim: Option<&'a str>,
    pub pos: Option<[i32; 3]>,
    pub slot: Option<i32>,
    pub depth: usize,
    pub file: &'a str,
    pub uuid: Option<&'a str>,
}

enum V<'a> {
    Null,
    Bool(bool),
    Num(f64),
    Str(Cow<'a, str>),
}

fn opt_str(s: Option<&str>) -> V<'_> {
    match s {
        Some(s) => V::Str(Cow::Borrowed(s)),
        None => V::Null,
    }
}

fn tag_value(t: Option<&Tag>) -> V<'_> {
    match t {
        None => V::Null,
        Some(Tag::String(s)) => V::Str(Cow::Borrowed(s)),
        Some(t) => match t.as_f64() {
            Some(n) => V::Num(n),
            None => {
                let mut s = String::new();
                nbt::write_snbt(t, &mut s);
                V::Str(Cow::Owned(s))
            }
        },
    }
}

fn value<'a>(v: &'a Val, cx: &'a EvalCtx<'a>) -> V<'a> {
    match v {
        Val::Num(n) => V::Num(*n),
        Val::Str(s) => V::Str(Cow::Borrowed(s)),
        Val::Bool(b) => V::Bool(*b),
        Val::Ench(id) => V::Num(
            cx.view.enchants().iter().filter(|e| &e.id == id).map(|e| e.level).max().unwrap_or(0) as f64,
        ),
        Val::Has(name) => V::Bool(match name.strip_prefix('!') {
            // Removal markers only exist in the raw component patch.
            Some(_) => cx.view.comps.map_or(false, |c| c.contains(name)),
            None => cx.view.has_component(name),
        }),
        Val::Comp(name) => tag_value(cx.view.component(name)),
        Val::Nbt(path) => tag_value(nbt::get_path(cx.view.c, path)),
        Val::Field(f) => match f {
            Field::Id => V::Str(Cow::Borrowed(cx.view.id)),
            Field::Count => V::Num(cx.view.count as f64),
            Field::Slot => cx.slot.map_or(V::Null, |s| V::Num(s as f64)),
            Field::Damage => cx.view.damage().map_or(V::Null, |d| V::Num(d as f64)),
            Field::Name => V::Str(Cow::Owned(cx.view.custom_name().unwrap_or_default())),
            Field::Depth => V::Num(cx.depth as f64),
            Field::Holder => opt_str(cx.holder),
            Field::Source => V::Str(Cow::Borrowed(cx.source)),
            Field::Dim => opt_str(cx.dim),
            Field::X => cx.pos.map_or(V::Null, |p| V::Num(p[0] as f64)),
            Field::Y => cx.pos.map_or(V::Null, |p| V::Num(p[1] as f64)),
            Field::Z => cx.pos.map_or(V::Null, |p| V::Num(p[2] as f64)),
            Field::Uuid => opt_str(cx.uuid),
            Field::File => V::Str(Cow::Borrowed(cx.file)),
            Field::MaxEnch => V::Num(cx.view.enchants().iter().map(|e| e.level).max().unwrap_or(0) as f64),
            Field::EnchCount => V::Num(cx.view.enchants().len() as f64),
            Field::Snbt => V::Str(Cow::Owned(nbt::snbt(cx.view.c))),
        },
    }
}

fn as_num(v: &V) -> Option<f64> {
    match v {
        V::Num(n) => Some(*n),
        V::Bool(b) => Some(*b as u8 as f64),
        V::Str(s) => s.trim().parse().ok(),
        V::Null => None,
    }
}

fn as_text<'v>(v: &'v V) -> Cow<'v, str> {
    match v {
        V::Str(s) => Cow::Borrowed(&**s),
        V::Num(n) => Cow::Owned(n.to_string()),
        V::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
        V::Null => Cow::Borrowed(""),
    }
}

fn loose_eq(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::Null, V::Null) => true,
        (V::Null, _) | (_, V::Null) => false,
        (V::Str(x), V::Str(y)) => short_id(x) == short_id(y),
        _ => match (as_num(a), as_num(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

fn order(a: &V, b: &V) -> Option<Ordering> {
    match (a, b) {
        (V::Null, _) | (_, V::Null) => None,
        (V::Str(x), V::Str(y)) => match (as_num(a), as_num(b)) {
            (Some(p), Some(q)) => p.partial_cmp(&q),
            _ => Some(short_id(x).cmp(short_id(y))),
        },
        _ => as_num(a)?.partial_cmp(&as_num(b)?),
    }
}

fn truthy(v: &V) -> bool {
    match v {
        V::Null => false,
        V::Bool(b) => *b,
        V::Num(n) => *n != 0.0,
        V::Str(s) => !s.is_empty(),
    }
}

fn eval(n: &Node, cx: &EvalCtx) -> bool {
    match n {
        Node::Or(a, b) => eval(a, cx) || eval(b, cx),
        Node::And(a, b) => eval(a, cx) && eval(b, cx),
        Node::Not(a) => !eval(a, cx),
        Node::Truthy(v) => truthy(&value(v, cx)),
        Node::Regex(v, re, negate) => re.is_match(&as_text(&value(v, cx))) != *negate,
        Node::In(v, items) => {
            let left = value(v, cx);
            items.iter().any(|i| loose_eq(&left, &value(i, cx)))
        }
        Node::Cmp(l, op, r) => {
            let (a, b) = (value(l, cx), value(r, cx));
            match op {
                Op::Eq => loose_eq(&a, &b),
                Op::Ne => !loose_eq(&a, &b),
                Op::Lt => order(&a, &b) == Some(Ordering::Less),
                Op::Le => matches!(order(&a, &b), Some(Ordering::Less | Ordering::Equal)),
                Op::Gt => order(&a, &b) == Some(Ordering::Greater),
                Op::Ge => matches!(order(&a, &b), Some(Ordering::Greater | Ordering::Equal)),
                Op::Like => id_glob(&as_text(&b), &as_text(&a)),
                Op::Contains => {
                    let needle = as_text(&b);
                    as_text(&a).contains(&*needle)
                }
                Op::Match | Op::NotMatch | Op::In => false, // compiled to other nodes
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::testutil::item;

    fn check(expr: &str, snbt: &str) -> bool {
        let c = item(snbt);
        let view = ItemView::new(&c).unwrap();
        let cx = EvalCtx {
            view: &view,
            holder: Some("minecraft:chest"),
            source: "block_entity",
            dim: Some("minecraft:the_nether"),
            pos: Some([10, 64, -30]),
            slot: Some(4),
            depth: 1,
            file: "region/r.0.-1.mca",
            uuid: None,
        };
        Filter::parse(expr).unwrap().matches(&cx)
    }

    const SWORD: &str = r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":255,"minecraft:looting":3},"minecraft:unbreakable":{},"minecraft:custom_name":"Admin Blade","minecraft:damage":7}}"#;

    #[test]
    fn globbing() {
        assert!(glob_match("*_sword", "diamond_sword"));
        assert!(glob_match("d*m?nd*", "diamond_sword"));
        assert!(!glob_match("*_axe", "diamond_sword"));
        assert!(glob_match("*", ""));
        assert!(id_glob("minecraft:*_egg", "zombie_spawn_egg"));
    }

    #[test]
    fn comparisons() {
        assert!(check("id == diamond_sword", SWORD));
        assert!(check("id = 'minecraft:diamond_sword' && count == 1", SWORD));
        assert!(check("ench(sharpness) > 5", SWORD));
        assert!(check("ench(minecraft:looting)>=3 and ench(smite)==0", SWORD));
        assert!(!check("ench(sharpness) <= 5", SWORD));
        assert!(check("max_ench == 255 and ench_count == 2", SWORD));
        assert!(check("id like *_sword", SWORD));
        assert!(check("id in (stick, diamond_sword)", SWORD));
        assert!(check("file like region/r.0.*", SWORD));
        assert!(!check("uuid like 1111*", SWORD));
        assert!(check("count>=1&&count<2", SWORD));
        assert!(check("x>-5 and z<=-30", SWORD));
        assert!(!check("id in (stick, stone)", SWORD));
        assert!(check("damage > 5 and damage < 8", SWORD));
        assert!(check("z < -5 and x >= 10 and dim == the_nether", SWORD));
        assert!(check("source == block_entity and holder == chest and depth > 0", SWORD));
    }

    #[test]
    fn functions_and_logic() {
        assert!(check("has(unbreakable)", SWORD));
        assert!(!check("has(lore)", SWORD));
        assert!(check("not has(lore) and (has(unbreakable) or count > 64)", SWORD));
        assert!(check("!has(lore) || false", SWORD));
        assert!(check("comp(damage) == 7", SWORD));
        assert!(check("nbt(components.minecraft:custom_name) == \"Admin Blade\"", SWORD));
        assert!(check("name contains Admin", SWORD));
        assert!(check("name ~ '(?i)^admin'", SWORD));
        assert!(check("name !~ dupe", SWORD));
        assert!(check("snbt ~ 'sharpness\":2[0-9]{2}'", SWORD));
        assert!(!check("uuid", SWORD));
        assert!(check("slot", SWORD));
        assert!(!check("nbt(components.missing) == 1", SWORD));
        assert!(check("nbt(components.missing) != 1", SWORD));
    }

    #[test]
    fn legacy_items() {
        let old = r#"{id:"minecraft:diamond_sword",Count:1b,tag:{Unbreakable:1b,Enchantments:[{id:"minecraft:sharpness",lvl:32767s}]}}"#;
        assert!(check("ench(sharpness) == 32767 and has(unbreakable)", old));
        assert!(check("nbt(tag.Enchantments[0].lvl) > 1000", old));
    }

    #[test]
    fn errors() {
        for bad in ["", "id ==", "bogus == 1", "id == (", "ench(sharpness", "id ~ '('", "zap(x) > 1", "id == a b"] {
            assert!(Filter::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }
}
