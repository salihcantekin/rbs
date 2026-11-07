//! `every_drawn_pair_has_a_contrast_rule` (themes spec, "Contrast"): reads the `.slint`
//! files, finds every text, border and icon colour token drawn on a surface colour
//! token, and fails when no contrast rule covers that pair. The token checks in
//! `rbs-ui` then hold every built-in and user theme to the pairs the UI really draws.
//!
//! The reading is a small structural model of Slint, not a compiler:
//!
//! - Elements nest by braces; a component's instances are expanded in place, with the
//!   instance's bindings over the component's defaults and the instance's children at
//!   `@children`. So `root.ink` and `background` resolve per instance.
//! - What is behind an element is the nearest ancestor with a `background`; a
//!   `transparent` branch looks further out. A popup or window starts afresh, and a
//!   colour that is not a theme token (a picked colour, a gradient, `with-alpha`) ends
//!   the search: no theme pair is drawn there.
//! - Conditions are kept as named atoms, so `kind == "primary" ? ...` in a background
//!   and in a text colour take the same branch, and `x == "a"` and `x == "b"` are never
//!   both true. Every consistent assignment is tried.
//!
//! Not seen: a sibling rectangle drawn under a text (only ancestors count) and colours
//! set from Rust. Those stay with the review checklist.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Tests fail by panicking.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::rc::Rc;

use rbs_ui::tokens::{CONTRAST_RULES, FIXED, LIVE_SURFACES, THEME_COLORS, THEME_CONTRAST_RULES};

/// Every `.slint` file of the app (`every_slint_file_is_read` checks the list).
const FILES: [(&str, &str); 8] = [
    ("app.slint", include_str!("../../ui/app.slint")),
    ("app_menu.slint", include_str!("../../ui/app_menu.slint")),
    (
        "appearance.slint",
        include_str!("../../ui/appearance.slint"),
    ),
    (
        "components.slint",
        include_str!("../../ui/components.slint"),
    ),
    (
        "context_menu.slint",
        include_str!("../../ui/context_menu.slint"),
    ),
    ("settings.slint", include_str!("../../ui/settings.slint")),
    ("shortcuts.slint", include_str!("../../ui/shortcuts.slint")),
    ("studio.slint", include_str!("../../ui/studio.slint")),
];

/// Foreground tokens with no contrast minimum. WCAG 2.2 1.4.3 and 1.4.11 exempt text
/// and boundaries of inactive controls, and `color.border.subtle` draws dividers and
/// resting outlines that never carry meaning alone (the design system's token table).
const EXEMPT: [&str; 2] = ["color.text.disabled", "color.border.subtle"];

/// Drawn pairs with no rule on purpose. The fixed live colour on `surface.raised` and
/// `.overlay` (2.92:1 and 2.54:1 in Dark) is not a theme's to fix, and live is always
/// shown with its words as well (U12; ADR-0057 review, item 1). The same holds for
/// the small live dot before "REC" on the selected scene row (`accent.subtle`).
const ACCEPTED: [(&str, &str); 3] = [
    ("color.state.live", "color.surface.raised"),
    ("color.state.live", "color.surface.overlay"),
    ("color.state.live", "color.accent.subtle"),
];

/// What a foreground colour draws: text needs 4.5:1, the others 3:1 (U7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Ink {
    Text,
    Border,
    Graphic,
}

// ---------------------------------------------------------------- reading

#[derive(Debug, Default)]
struct Elem {
    ty: String,
    /// Where it starts (`file:line`).
    at: Rc<str>,
    /// The condition of an `if cond: Type { }` element.
    cond: Option<Rc<str>>,
    bindings: Vec<Binding>,
    /// Properties declared here.
    props: BTreeSet<String>,
    children: Vec<Item>,
}

#[derive(Debug, Clone)]
struct Binding {
    name: Rc<str>,
    expr: Rc<str>,
    at: Rc<str>,
}

#[derive(Debug)]
enum Item {
    Elem(Elem),
    /// `@children`.
    Children,
}

#[derive(Debug)]
struct Component {
    root: Elem,
    has_slot: bool,
}

struct Reader<'a> {
    file: &'static str,
    s: &'a [u8],
    i: usize,
    /// Positions of the newlines.
    lines: Vec<usize>,
}

/// The text with comments blanked out (newlines kept, strings untouched).
fn without_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            out.push(b[i]);
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    out.push(b[i]);
                    i += 1;
                }
                out.push(b[i]);
                i += 1;
            }
            if i < b.len() {
                out.push(b[i]);
                i += 1;
            }
        } else if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            while i < b.len() && !b[i..].starts_with(b"*/") {
                out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            out.extend_from_slice(b"  ");
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).expect("blanking keeps UTF-8")
}

impl Reader<'_> {
    fn line(&self, at: usize) -> usize {
        1 + self.lines.partition_point(|l| *l < at)
    }

    fn skip_string(&mut self) {
        self.i += 1;
        while self.i < self.s.len() && self.s[self.i] != b'"' {
            if self.s[self.i] == b'\\' {
                self.i += 1;
            }
            self.i += 1;
        }
        self.i += 1;
    }

    /// Text up to the next `;`, `{` or `}` outside brackets and strings, and that
    /// terminator (`0` at the end). The terminator is not consumed.
    fn statement(&mut self) -> (usize, String, u8) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
        let start = self.i;
        let mut depth = 0i32;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'"' => {
                    self.skip_string();
                    continue;
                }
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                c @ (b';' | b'{' | b'}') if depth == 0 => {
                    let text = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
                    return (start, text, c);
                }
                _ => {}
            }
            self.i += 1;
        }
        let text = String::from_utf8_lossy(&self.s[start..]).into_owned();
        (start, text, 0)
    }

    /// Skips a balanced `{ ... }` starting at the current `{`.
    fn skip_block(&mut self) {
        let mut depth = 0;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'"' => {
                    self.skip_string();
                    continue;
                }
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += 1;
                        return;
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
    }

    /// An element body after its `{`, up to and including its `}`.
    fn body(&mut self, elem: &mut Elem) {
        loop {
            let (start, mut text, end) = self.statement();
            if text.contains("@children") {
                text = text.replace("@children", " ");
                elem.children.push(Item::Children);
            }
            let text = text.trim().to_owned();
            match end {
                b';' => {
                    self.i += 1;
                    self.binding(elem, start, &text);
                }
                b'}' | 0 => {
                    self.binding(elem, start, &text);
                    self.i += 1;
                    return;
                }
                _ => {
                    // `{`: a nested element, a code block or a block expression.
                    if is_code(&text) || text.ends_with(':') || !is_element(&text) {
                        self.skip_block();
                        continue;
                    }
                    self.i += 1;
                    let mut child = Elem {
                        ty: element_type(&text),
                        cond: if_condition(&text).map(Into::into),
                        at: format!("{}:{}", self.file, self.line(start)).into(),
                        ..Elem::default()
                    };
                    self.body(&mut child);
                    elem.children.push(Item::Elem(child));
                }
            }
        }
    }

    fn binding(&self, elem: &mut Elem, start: usize, text: &str) {
        if text.is_empty() || text.contains("<=>") || text.starts_with("callback") {
            return;
        }
        let at: Rc<str> = format!("{}:{}", self.file, self.line(start)).into();
        let words: Vec<&str> = text.split_whitespace().collect();
        if let Some(p) = words
            .iter()
            .position(|w| *w == "property" || w.starts_with("property<"))
        {
            // `in property <color> ink: Theme.text-primary`
            let rest = &text[text.find('>').map_or(text.len(), |i| i + 1)..];
            let _ = p;
            let (name, expr) = match rest.split_once(':') {
                Some((n, e)) => (n.trim(), Some(e.trim())),
                None => (rest.trim(), None),
            };
            if !name.is_empty() {
                elem.props.insert(name.to_owned());
                if let Some(expr) = expr {
                    elem.bindings.push(Binding {
                        name: name.into(),
                        expr: expr.into(),
                        at,
                    });
                }
            }
            return;
        }
        if let Some((name, expr)) = text.split_once(':') {
            let name = name.trim();
            if is_ident(name) {
                elem.bindings.push(Binding {
                    name: name.into(),
                    expr: expr.trim().into(),
                    at,
                });
            }
        }
    }

    /// The whole file: its components by name.
    fn file(&mut self, out: &mut BTreeMap<String, Component>) {
        loop {
            let (_, text, end) = self.statement();
            match end {
                0 => return,
                b';' | b'}' => self.i += 1,
                _ => {
                    let words: Vec<&str> = text.split_whitespace().collect();
                    let Some(k) = words.iter().position(|w| *w == "component") else {
                        self.skip_block();
                        continue;
                    };
                    let name = words[k + 1].to_owned();
                    let base = match words.iter().position(|w| *w == "inherits") {
                        Some(i) => words[i + 1].to_owned(),
                        None => "Empty".to_owned(),
                    };
                    self.i += 1;
                    let mut root = Elem {
                        ty: base,
                        at: format!("{}:{name}", self.file).into(),
                        ..Elem::default()
                    };
                    self.body(&mut root);
                    let has_slot = has_slot(&root);
                    out.insert(name, Component { root, has_slot });
                }
            }
        }
    }
}

fn has_slot(e: &Elem) -> bool {
    e.children.iter().any(|c| match c {
        Item::Children => true,
        Item::Elem(e) => has_slot(e),
    })
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_code(header: &str) -> bool {
    let first = header.split_whitespace().next().unwrap_or("");
    header.ends_with("=>")
        || header.contains("=> ")
        || matches!(
            first,
            "function" | "pure" | "public" | "animate" | "states" | "transitions"
        )
}

fn element_type(header: &str) -> String {
    let t = header
        .rsplit(|c: char| c.is_whitespace() || c == ':' || c == '=')
        .next()
        .unwrap_or("");
    t.to_owned()
}

/// `cond` of an `if cond: id := Type` header.
fn if_condition(header: &str) -> Option<String> {
    let rest = header.strip_prefix("if ")?;
    let b = rest.as_bytes();
    let colon = (0..b.len())
        .rev()
        .find(|&i| b[i] == b':' && b.get(i + 1) != Some(&b'='))?;
    Some(rest[..colon].trim().to_owned())
}

fn is_element(header: &str) -> bool {
    element_type(header)
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
}

// ---------------------------------------------------------------- expressions

#[derive(Debug)]
enum Ex {
    /// `cond ? then : else`.
    Tern(Cond, Rc<Ex>, Rc<Ex>),
    /// A theme colour token (`color.text.primary`).
    Tok(&'static str),
    Transparent,
    /// A string, number, boolean or enum value.
    Lit(Rc<str>),
    /// A property of the component the expression is in.
    Prop(Rc<str>),
    /// Anything else: a gradient, a tint, a picked colour, another element's property.
    Other,
}

/// A condition, with its atoms numbered in [`Atoms`].
#[derive(Debug)]
enum Cond {
    Any(Vec<Cond>),
    All(Vec<Cond>),
    Not(Box<Cond>),
    Atom(u32),
}

/// One atom of the conditions: `kind == "primary"`, `checked`, `touch.has-hover`.
#[derive(Debug)]
struct AtomInfo {
    /// The property or path it reads (left of `==`, or the whole atom).
    lhs: Rc<str>,
    /// Right of `==`.
    rhs: Option<Rc<str>>,
}

#[derive(Default)]
struct Atoms {
    ids: HashMap<String, u32>,
    info: Vec<AtomInfo>,
}

impl Atoms {
    fn id(&mut self, text: String, lhs: &str, rhs: Option<&str>) -> u32 {
        if let Some(id) = self.ids.get(&text) {
            return *id;
        }
        let id = self.info.len() as u32;
        self.info.push(AtomInfo {
            lhs: lhs.into(),
            rhs: rhs.map(Into::into),
        });
        self.ids.insert(text, id);
        id
    }
}

fn strip_parens(mut s: &str) -> &str {
    loop {
        s = s.trim();
        if !(s.starts_with('(') && s.ends_with(')')) {
            return s;
        }
        // Only when the first parenthesis closes at the end.
        let mut depth = 0;
        for (i, c) in s.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && i != s.len() - 1 {
                        return s;
                    }
                }
                _ => {}
            }
        }
        s = &s[1..s.len() - 1];
    }
}

/// Calls `f` for each byte outside strings with the bracket depth before it; `f`
/// returns how many bytes to skip, or `None` to stop.
fn scan(s: &str, mut f: impl FnMut(usize, i32) -> Option<usize>) {
    let b = s.as_bytes();
    let (mut depth, mut i) = (0i32, 0);
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }
        match f(i, depth) {
            Some(skip) => i += skip.max(1),
            None => return,
        }
    }
}

/// Byte positions of the top-level `?` and its `:`.
fn ternary(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let (mut q, mut pending, mut found) = (None, 0, None);
    scan(s, |i, depth| {
        if depth == 0 && b[i] == b'?' {
            if q.is_none() {
                q = Some(i);
            } else {
                pending += 1;
            }
        } else if depth == 0 && b[i] == b':' && q.is_some() {
            if pending == 0 {
                found = q.map(|q| (q, i));
                return None;
            }
            pending -= 1;
        }
        Some(1)
    });
    found
}

/// `s` split at each top-level `sep`.
fn split_top<'s>(s: &'s str, sep: &str) -> Vec<&'s str> {
    let mut cuts = Vec::new();
    scan(s, |i, depth| {
        if depth == 0 && s.as_bytes()[i..].starts_with(sep.as_bytes()) {
            cuts.push(i);
            return Some(sep.len());
        }
        Some(1)
    });
    let mut out = Vec::new();
    let mut start = 0;
    for c in cuts {
        out.push(&s[start..c]);
        start = c + sep.len();
    }
    out.push(&s[start..]);
    out
}

fn is_literal(v: &str) -> bool {
    (v.starts_with('"') && v.ends_with('"') && v.len() >= 2 && v.matches('"').count() == 2)
        || v.parse::<f64>().is_ok()
        || v == "true"
        || v == "false"
        || (v.contains('.')
            && v.split('.').all(is_ident)
            && v.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && !v.starts_with("Theme.")
            && !v.starts_with("Tokens."))
}

fn local(s: &str) -> &str {
    s.strip_prefix("root.")
        .or_else(|| s.strip_prefix("self."))
        .unwrap_or(s)
}

struct Parser<'a> {
    theme: &'a HashMap<String, &'static str>,
    atoms: Atoms,
}

impl Parser<'_> {
    fn ex(&mut self, s: &str) -> Ex {
        let s = strip_parens(s);
        if let Some((q, c)) = ternary(s) {
            return Ex::Tern(
                self.cond(&s[..q]),
                Rc::new(self.ex(&s[q + 1..c])),
                Rc::new(self.ex(&s[c + 1..])),
            );
        }
        if s == "transparent" {
            return Ex::Transparent;
        }
        if is_literal(s) {
            return Ex::Lit(s.into());
        }
        if let Some(name) = s.strip_prefix("Theme.")
            && let Some(token) = self.theme.get(name)
        {
            return Ex::Tok(token);
        }
        if is_ident(local(s)) {
            return Ex::Prop(local(s).into());
        }
        Ex::Other
    }

    fn cond(&mut self, s: &str) -> Cond {
        let c = strip_parens(s);
        let any = split_top(c, "||");
        if any.len() > 1 {
            return Cond::Any(any.into_iter().map(|p| self.cond(p)).collect());
        }
        let all = split_top(c, "&&");
        if all.len() > 1 {
            return Cond::All(all.into_iter().map(|p| self.cond(p)).collect());
        }
        if let Some(rest) = c.strip_prefix('!')
            && !rest.starts_with('=')
        {
            return Cond::Not(Box::new(self.cond(rest)));
        }
        for (op, negate) in [("!=", true), ("==", false)] {
            if let Some((l, r)) = c.split_once(op) {
                let (l, r) = (local(l.trim()), strip_parens(r));
                let id = self.atoms.id(format!("{l} == {r}"), l, Some(r));
                let atom = Cond::Atom(id);
                return if negate {
                    Cond::Not(Box::new(atom))
                } else {
                    atom
                };
            }
        }
        let l = local(c);
        Cond::Atom(self.atoms.id(l.to_owned(), l, None))
    }
}

// ---------------------------------------------------------------- expansion

/// A component instance's property values: each with the instance it is written in.
#[derive(Default)]
struct Env {
    props: HashMap<Rc<str>, (Rc<str>, usize)>,
}

struct Slot<'a> {
    items: &'a [Item],
    env: usize,
    parent: Option<Rc<Slot<'a>>>,
}

/// One foreground drawn over a stack of backgrounds (outermost first).
struct Drawn {
    ink: Ink,
    fg: (Rc<str>, usize),
    behind: Vec<(Rc<str>, usize)>,
    /// The `if` conditions around it.
    guards: Vec<(Rc<str>, usize)>,
    at: Rc<str>,
}

struct Expander<'a> {
    comps: &'a BTreeMap<String, Component>,
    envs: Vec<Env>,
    drawn: Vec<Drawn>,
    /// The component instances being expanded.
    path: Vec<Rc<str>>,
    /// The `if` conditions of the elements being walked.
    guards: Vec<(Rc<str>, usize)>,
    /// For each element being walked, how much of the background stack is outside it.
    bases: Vec<usize>,
}

/// Components that draw a ring on the edge of the control they are in (the focus ring).
const EDGE_RINGS: [&str; 1] = ["FocusRing"];

const OPAQUE_ROOTS: [&str; 3] = ["Window", "PopupWindow", "Dialog"];

impl<'a> Expander<'a> {
    fn walk(
        &mut self,
        elem: &'a Elem,
        env: usize,
        over: &[(Binding, usize)],
        behind: &[(Rc<str>, usize)],
        slot: Option<Rc<Slot<'a>>>,
        depth: usize,
    ) {
        if let Some(c) = &elem.cond {
            self.guards.push((c.clone(), env));
        }
        self.walk_guarded(elem, env, over, behind, slot, depth);
        if elem.cond.is_some() {
            self.guards.pop();
        }
    }

    fn walk_guarded(
        &mut self,
        elem: &'a Elem,
        env: usize,
        over: &[(Binding, usize)],
        behind: &[(Rc<str>, usize)],
        slot: Option<Rc<Slot<'a>>>,
        depth: usize,
    ) {
        assert!(depth < 64, "component nesting too deep at {}", elem.ty);
        // The element's bindings, each with the instance it is written in.
        let mut eff: Vec<(Binding, usize)> = elem
            .bindings
            .iter()
            .filter(|b| !over.iter().any(|(o, _)| o.name == b.name))
            .map(|b| (b.clone(), env))
            .collect();
        eff.extend(over.iter().cloned());

        let comps = self.comps;
        if let Some(comp) = comps.get(&elem.ty) {
            let inner = self.envs.len();
            self.envs.push(Env::default());
            let child_slot = Rc::new(Slot {
                items: &elem.children,
                env,
                parent: slot,
            });
            // A ring drawn on its control's edge stands against what is around the control.
            let behind = if EDGE_RINGS.contains(&elem.ty.as_str()) {
                &behind[..self.bases.last().copied().unwrap_or(0).min(behind.len())]
            } else {
                behind
            };
            self.path.push(elem.at.clone());
            self.walk(
                &comp.root,
                inner,
                &eff,
                behind,
                Some(child_slot.clone()),
                depth + 1,
            );
            self.path.pop();
            if !comp.has_slot {
                // No `@children`: they go after the root's own children.
                let mut stack = behind.to_vec();
                if let Some((b, e)) = eff.iter().find(|(b, _)| &*b.name == "background") {
                    stack.push((b.expr.clone(), *e));
                }
                self.walk_slot(&child_slot, &stack, depth);
            }
            return;
        }

        for (b, e) in &eff {
            if elem.props.contains(&*b.name) {
                self.envs[env]
                    .props
                    .insert(b.name.clone(), (b.expr.clone(), *e));
            }
        }
        for (b, e) in &eff {
            let ink = match &*b.name {
                "color" => Ink::Text,
                "border-color" => Ink::Border,
                "stroke" | "fill" => Ink::Graphic,
                "selection-foreground-color" => {
                    if let Some((sb, se)) = eff
                        .iter()
                        .find(|(s, _)| &*s.name == "selection-background-color")
                    {
                        self.drawn.push(Drawn {
                            ink: Ink::Text,
                            fg: (b.expr.clone(), *e),
                            behind: vec![(sb.expr.clone(), *se)],
                            guards: self.guards.clone(),
                            at: self.place(&b.at),
                        });
                    }
                    continue;
                }
                _ => continue,
            };
            self.drawn.push(Drawn {
                ink,
                fg: (b.expr.clone(), *e),
                behind: behind.to_vec(),
                guards: self.guards.clone(),
                at: self.place(&b.at),
            });
        }
        let mut stack = if OPAQUE_ROOTS.contains(&elem.ty.as_str()) {
            Vec::new()
        } else {
            behind.to_vec()
        };
        self.bases.push(stack.len());
        if let Some((b, e)) = eff.iter().find(|(b, _)| &*b.name == "background") {
            stack.push((b.expr.clone(), *e));
        }
        self.walk_children(elem, env, &stack, &slot, depth);
        self.bases.pop();
    }

    fn walk_children(
        &mut self,
        elem: &'a Elem,
        env: usize,
        stack: &[(Rc<str>, usize)],
        slot: &Option<Rc<Slot<'a>>>,
        depth: usize,
    ) {
        for child in &elem.children {
            match child {
                Item::Elem(c) => self.walk(c, env, &[], stack, slot.clone(), depth + 1),
                Item::Children => {
                    if let Some(s) = slot {
                        self.walk_slot(s, stack, depth);
                    }
                }
            }
        }
    }

    /// `at`, and the component instances it is drawn through, innermost first.
    fn place(&self, at: &str) -> Rc<str> {
        let mut s = at.to_owned();
        for p in self.path.iter().rev() {
            s.push_str(" < ");
            s.push_str(p);
        }
        s.into()
    }

    fn walk_slot(&mut self, slot: &Rc<Slot<'a>>, behind: &[(Rc<str>, usize)], depth: usize) {
        for item in slot.items {
            match item {
                Item::Elem(c) => {
                    self.walk(c, slot.env, &[], behind, slot.parent.clone(), depth + 1)
                }
                Item::Children => {
                    if let Some(p) = &slot.parent {
                        self.walk_slot(p, behind, depth + 1);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- evaluation

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Tok(&'static str),
    Transparent,
    Lit(Rc<str>),
    Other,
}

/// Atom values tried so far: (instance, atom, value).
type Assign = Vec<(usize, u32, bool)>;

/// The atom (in an instance) that must be decided before evaluation can go on.
type Need = (usize, u32);

fn get(assign: &Assign, env: usize, atom: u32) -> Option<bool> {
    assign
        .iter()
        .find(|(e, a, _)| *e == env && *a == atom)
        .map(|(_, _, v)| *v)
}

struct Eval<'a> {
    envs: &'a [Env],
    parser: Parser<'a>,
    /// Parsed expressions, by the address of their text.
    exprs: HashMap<*const u8, Rc<Ex>>,
    conds: HashMap<*const u8, Rc<Cond>>,
}

impl Eval<'_> {
    fn ex(&mut self, s: &Rc<str>) -> Rc<Ex> {
        if let Some(e) = self.exprs.get(&s.as_ptr()) {
            return e.clone();
        }
        let e = Rc::new(self.parser.ex(s));
        self.exprs.insert(s.as_ptr(), e.clone());
        e
    }

    fn cond(&mut self, s: &Rc<str>) -> Rc<Cond> {
        if let Some(c) = self.conds.get(&s.as_ptr()) {
            return c.clone();
        }
        let c = Rc::new(self.parser.cond(s));
        self.conds.insert(s.as_ptr(), c.clone());
        c
    }

    /// The value under `assign`, or the atom that must be decided first.
    fn value(&mut self, ex: &Ex, env: usize, assign: &Assign, depth: usize) -> Result<Val, Need> {
        match ex {
            Ex::Tern(c, a, b) => {
                let branch = if self.truth(c, env, assign, depth)? {
                    a
                } else {
                    b
                };
                self.value(branch, env, assign, depth)
            }
            Ex::Tok(t) => Ok(Val::Tok(t)),
            Ex::Lit(l) => Ok(Val::Lit(l.clone())),
            Ex::Transparent => Ok(Val::Transparent),
            Ex::Other => Ok(Val::Other),
            Ex::Prop(p) => {
                let Some((expr, e)) = self.envs[env].props.get(p).cloned() else {
                    return Ok(Val::Other);
                };
                if depth > 16 {
                    return Ok(Val::Other);
                }
                let ex = self.ex(&expr);
                self.value(&ex, e, assign, depth + 1)
            }
        }
    }

    /// Whether a condition holds; each atom is decided by the properties it reads or
    /// else by `assign`.
    fn truth(&mut self, c: &Cond, env: usize, assign: &Assign, depth: usize) -> Result<bool, Need> {
        match c {
            Cond::Any(parts) => {
                for p in parts {
                    if self.truth(p, env, assign, depth)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Cond::All(parts) => {
                for p in parts {
                    if !self.truth(p, env, assign, depth)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Cond::Not(p) => Ok(!self.truth(p, env, assign, depth)?),
            Cond::Atom(id) => {
                if depth < 16
                    && let Some(v) = self.decided(*id, env, assign, depth)?
                {
                    return Ok(v);
                }
                get(assign, env, *id).ok_or((env, *id))
            }
        }
    }

    /// An atom that the properties it reads decide: `kind == "primary"` when `kind`
    /// comes to a literal, `record` when it comes to `true`, `false` or a condition.
    fn decided(
        &mut self,
        id: u32,
        env: usize,
        assign: &Assign,
        depth: usize,
    ) -> Result<Option<bool>, Need> {
        let info = &self.parser.atoms.info[id as usize];
        let (lhs, rhs) = (info.lhs.clone(), info.rhs.clone());
        let Some((expr, penv)) = self.envs[env].props.get(&lhs).cloned() else {
            return Ok(None);
        };
        let ex = self.ex(&expr);
        let v = self.value(&ex, penv, assign, depth + 1)?;
        Ok(match (rhs, v) {
            (Some(r), Val::Lit(v)) => Some(v == r),
            (Some(_), _) => None,
            (None, Val::Lit(v)) if &*v == "true" => Some(true),
            (None, Val::Lit(v)) if &*v == "false" => Some(false),
            (None, Val::Other) if !matches!(*ex, Ex::Tern(..)) => {
                let c = self.cond(&expr);
                Some(self.truth(&c, penv, assign, depth + 1)?)
            }
            (None, _) => None,
        })
    }

    /// The theme colour pair under `assign`: the foreground and what is behind it.
    /// `None` when either is not a theme colour or an `if` around it does not hold;
    /// the `if`s are checked last, and not at all for a pair already in `seen`.
    fn pair(
        &mut self,
        d: &Drawn,
        assign: &Assign,
        seen: &impl Fn(&'static str, &'static str) -> bool,
    ) -> Result<Option<(&'static str, &'static str)>, Need> {
        let fg_ex = self.ex(&d.fg.0);
        let Val::Tok(fg) = self.value(&fg_ex, d.fg.1, assign, 0)? else {
            return Ok(None);
        };
        let mut bg = None;
        for (expr, env) in d.behind.iter().rev() {
            let ex = self.ex(expr);
            match self.value(&ex, *env, assign, 0)? {
                Val::Transparent => continue,
                Val::Tok(t) => bg = Some(t),
                _ => {}
            }
            break;
        }
        let Some(bg) = bg else {
            return Ok(None);
        };
        if seen(fg, bg) {
            return Ok(None);
        }
        for (cond, env) in &d.guards {
            let c = self.cond(cond);
            if !self.truth(&c, *env, assign, 0)? {
                return Ok(None);
            }
        }
        Ok(Some((fg, bg)))
    }

    /// `x == a` and `x == b` with `a != b` cannot both hold in one instance.
    fn consistent(&self, assign: &Assign, (env, id): Need) -> bool {
        let info = &self.parser.atoms.info;
        let me = &info[id as usize];
        let Some(r) = &me.rhs else {
            return true;
        };
        !assign.iter().any(|(e, a, v)| {
            let other = &info[*a as usize];
            *v && *e == env && other.lhs == me.lhs && other.rhs.as_ref().is_some_and(|r2| r2 != r)
        })
    }
}

/// Every (ink, foreground, background) pair the UI draws, with a place it is drawn.
fn drawn_pairs() -> BTreeMap<(Ink, &'static str, &'static str), Rc<str>> {
    let theme: HashMap<String, &'static str> = THEME_COLORS
        .iter()
        .copied()
        .chain(FIXED.iter().map(|(n, _)| *n))
        .map(|n| (n.trim_start_matches("color.").replace('.', "-"), n))
        .collect();
    let mut comps = BTreeMap::new();
    for (file, src) in FILES {
        let text = without_comments(src);
        let mut r = Reader {
            file,
            s: text.as_bytes(),
            i: 0,
            lines: text.match_indices('\n').map(|(i, _)| i).collect(),
        };
        r.file(&mut comps);
    }
    let mut used = BTreeSet::new();
    fn types(e: &Elem, out: &mut BTreeSet<String>) {
        for c in &e.children {
            if let Item::Elem(c) = c {
                out.insert(c.ty.clone());
                types(c, out);
            }
        }
    }
    for c in comps.values() {
        used.insert(c.root.ty.clone());
        types(&c.root, &mut used);
    }
    let mut x = Expander {
        comps: &comps,
        envs: Vec::new(),
        drawn: Vec::new(),
        path: Vec::new(),
        guards: Vec::new(),
        bases: Vec::new(),
    };
    // Windows and anything only Rust shows are the roots.
    for (_, comp) in comps.iter().filter(|(n, _)| !used.contains(*n)) {
        let env = x.envs.len();
        x.envs.push(Env::default());
        x.walk(&comp.root, env, &[], &[], None, 0);
    }
    let Expander { envs, drawn, .. } = x;
    let mut ev = Eval {
        envs: &envs,
        parser: Parser {
            theme: &theme,
            atoms: Atoms::default(),
        },
        exprs: HashMap::new(),
        conds: HashMap::new(),
    };
    let mut out = BTreeMap::new();
    for d in &drawn {
        let mut work = vec![Assign::new()];
        let mut tried = 0;
        while let Some(a) = work.pop() {
            tried += 1;
            assert!(tried < 1 << 14, "too many conditions around {}", d.at);
            let seen = |fg, bg| out.contains_key(&(d.ink, fg, bg));
            match ev.pair(d, &a, &seen) {
                Ok(Some((fg, bg))) => {
                    out.insert((d.ink, fg, bg), d.at.clone());
                }
                Ok(None) => {}
                Err(need) => {
                    if ev.consistent(&a, need) {
                        let mut yes = a.clone();
                        yes.push((need.0, need.1, true));
                        work.push(yes);
                    }
                    let mut no = a;
                    no.push((need.0, need.1, false));
                    work.push(no);
                }
            }
        }
    }
    out
}

/// The smallest ratio a rule asks of `fg` on `bg`, when one does.
fn rule(fg: &str, bg: &str) -> Option<f64> {
    let fixed = [
        ("color.text.on-live", "color.state.live", 4.5),
        ("color.caption.close-text", "color.caption.close-hover", 4.5),
    ]
    .into_iter()
    .chain(LIVE_SURFACES.iter().map(|s| ("color.state.live", *s, 3.0)));
    CONTRAST_RULES
        .iter()
        .chain(THEME_CONTRAST_RULES)
        .map(|(f, b, min, _)| (*f, *b, *min))
        .chain(fixed)
        .filter(|(f, b, _)| *f == fg && *b == bg)
        .map(|(_, _, min)| min)
        .reduce(f64::max)
}

#[test]
fn every_drawn_pair_has_a_contrast_rule() {
    let pairs = drawn_pairs();
    // The reading finds the pairs it must: a guard against a reader that sees nothing.
    for (ink, fg, bg) in [
        (Ink::Text, "color.text.primary", "color.surface.raised"),
        (Ink::Text, "color.text.secondary", "color.surface.sunken"),
        (Ink::Text, "color.text.inverse", "color.accent"),
        (Ink::Text, "color.text.on-live", "color.state.live"),
        (Ink::Border, "color.border.focus", "color.surface.raised"),
    ] {
        assert!(
            pairs.contains_key(&(ink, fg, bg)),
            "the .slint reading no longer finds {fg} on {bg}"
        );
    }
    let mut missing = String::new();
    for ((ink, fg, bg), at) in &pairs {
        if EXEMPT.contains(fg) || fg == bg || ACCEPTED.contains(&(*fg, *bg)) {
            continue;
        }
        let need = if *ink == Ink::Text { 4.5 } else { 3.0 };
        match rule(fg, bg) {
            Some(min) if min >= need => {}
            Some(min) => {
                let _ = writeln!(
                    missing,
                    "{ink:?} {fg} on {bg} ({at}): rule asks {min}, needs {need}"
                );
            }
            None => {
                let _ = writeln!(
                    missing,
                    "{ink:?} {fg} on {bg} ({at}): no rule, needs {need}"
                );
            }
        }
    }
    assert!(
        missing.is_empty(),
        "drawn pairs without a contrast rule:\n{missing}"
    );
}

#[test]
fn every_slint_file_is_read() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/ui");
    let mut on_disk: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".slint"))
        .collect();
    on_disk.sort();
    let listed: Vec<&str> = FILES.iter().map(|(n, _)| *n).collect();
    assert_eq!(on_disk, listed, "add the new .slint file to FILES");
}
