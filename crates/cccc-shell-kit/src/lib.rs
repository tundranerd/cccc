//! Shared lowering code for the shell-family adapters (`cccc-sh` and
//! `cccc-zsh`).
//!
//! The `tree-sitter-zsh` grammar is a reworking that **starts from the bash
//! grammar**: its control-flow skeleton (`if`/`elif`/`else`, `for`, `while`,
//! `case`, `function_definition`, `pipeline`, `list`, `do_group`, …) is copied
//! from `tree-sitter-bash` essentially verbatim, and it only adds a larger
//! lexical/expansion layer plus a handful of zsh-only statement kinds. This
//! crate lowers the shared skeleton once, so `cccc-sh` and `cccc-zsh` don't
//! duplicate it.
//!
//! The main type is [`SharedBuilder`]. Each adapter parses with its own grammar
//! and tells the builder which dialect it is lowering via [`Language`]. Most of
//! [`SharedBuilder::visit`] handles what `sh`/`bash` and `zsh` share; a few
//! match arms are gated on [`Language::Zsh`] for constructs only zsh's grammar
//! produces (`repeat`, the terse `for x (…)`, a standalone `select`, `coproc`,
//! and `{ … } always { … }`).
//!
//! This crate has no scoring logic; that lives in `cccc_core::engine`. It
//! depends only on `cccc-core` and `tree-sitter`, not on a specific grammar
//! crate. Each adapter brings its own grammar (`tree-sitter-bash` /
//! `tree-sitter-zsh`) and calls [`to_ir`].
//!
//! ## Shell-to-IR mapping notes
//!
//! - `function_definition` (`foo() { … }`, `function foo { … }`, and
//!   `function foo() { … }`) → [`Node::Function`] (`kind` `"function"`).
//!   Shell functions nest, so a function inside another is its own unit. zsh's
//!   anonymous `() { … }` (no name field) reports `<function>`.
//! - `if` / `elif` / `else` → [`Node::Branch`], with `elif` chained as a nested
//!   `Branch` in `alternate` so it scores flat (like every other adapter). The
//!   grammar does not field the `then` boundary, so it is found by splitting
//!   the clause's children at the `then` keyword.
//! - `for` / `select` / `while` / `until` / C-style `for (( … ))`, plus zsh's
//!   `repeat n do … done` and terse `for x (…) …` → [`Node::Loop`]. The
//!   condition/initializer/update are walked inside the loop, matching how the
//!   other adapters score loop headers.
//! - `case` → [`Node::Switch`]. Each `case_item` is a [`SwitchCase`]; a
//!   catch-all `*)` arm is marked `is_default` (it is not a decision point).
//!   zsh's `;|` fallthrough is a terminator, not a decision, so it changes
//!   nothing.
//! - `&&` / `||` runs (shell `list` nodes, and `binary_expression`s inside
//!   `[[ … ]]` / `(( … ))`) → **folded** [`Node::Logical`], so `a && b && c` is
//!   one node with three operands. `[ a -a b -o c ]` also folds, since `-a`/`-o`
//!   in a `binary_expression` are the POSIX logical operators (a unary `-a`
//!   is file-existence and is not a `binary_expression`).
//! - `?:` inside `(( … ))` → [`Node::Conditional`].
//! - a simple `command` → [`Node::Call`] (callee name for recursion detection).
//!   `break N` / `continue N` with `N > 1` → [`Node::Jump`] `labeled: true`
//!   (one flat cognitive point, mirroring a labelled jump); plain
//!   `break`/`continue`/`return` score nothing.
//! - `subshell`, `compound_statement`, `pipeline`, `command_substitution`,
//!   `process_substitution`, `redirected_statement`, variable assignments, and
//!   heredocs are transparent. `heredoc_content` is data and carries no code,
//!   while a `command_substitution` inside a heredoc body is still reached.
//!   zsh's `{ … } always { … }`, `coproc`, and `compound_statement_no_always`
//!   are transparent too — an `always` block is a `finally`, which is not a
//!   decision point.
//! - `negated_command` (`! cmd`) adds nothing (mirroring Python's `not` and
//!   SonarSource). Shell has no exception construct, so [`Node::Catch`] is
//!   never emitted (`trap` is an ordinary command).

use cccc_core::ir::{LogicalOp, Node, SwitchCase};
use tree_sitter::Node as TsNode;

/// Which shell dialect is being lowered. Only zsh-exclusive match arms consult
/// it; everything shared is dialect-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// POSIX `sh` / `bash` (parsed with `tree-sitter-bash`).
    Sh,
    /// `zsh` (parsed with `tree-sitter-zsh`).
    Zsh,
}

/// Collect the 1-based lines of every `ERROR`/`MISSING` node so a partially
/// parsed file surfaces its syntax problems (deduplicated, order preserved).
pub fn collect_errors(node: TsNode, out: &mut Vec<String>) {
    let mut cursor = node.walk();
    if node.is_error() || node.is_missing() {
        let msg = format!("syntax error at line {}", node.start_position().row + 1);
        if !out.contains(&msg) {
            out.push(msg);
        }
    }
    for child in node.children(&mut cursor) {
        collect_errors(child, out);
    }
}

/// Parse `source` with `grammar` and lower it with a [`SharedBuilder`] tagged
/// `lang`, returning the module-level IR plus any syntax-error messages.
/// tree-sitter always yields a tree (it recovers from errors by inserting
/// `ERROR`/`MISSING` nodes), so a partial parse is still lowered and its error
/// locations are reported alongside. This is the whole per-adapter parsing
/// step, so an adapter is just its grammar dependency plus a `DEFAULT_EXTS`.
pub fn to_ir(
    grammar: tree_sitter::Language,
    lang: Language,
    source: &str,
) -> (Vec<Node>, Vec<String>) {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar).is_err() {
        return (Vec::new(), vec!["failed to load grammar".to_string()]);
    }
    let Some(tree) = parser.parse(source, None) else {
        return (Vec::new(), vec!["failed to parse source".to_string()]);
    };

    let src = source.as_bytes();
    let mut errors = Vec::new();
    collect_errors(tree.root_node(), &mut errors);

    let mut builder = SharedBuilder::new(src, lang);
    builder.visit(tree.root_node());
    (builder.finish(), errors)
}

/// Assembles the IR tree while an explicit recursion walks the tree-sitter CST.
pub struct SharedBuilder<'a> {
    /// Source bytes, for extracting identifier text.
    src: &'a [u8],
    /// Stack of node collectors. `stack.last_mut()` receives emitted nodes;
    /// structural nodes push a fresh collector for their body, then pop it.
    stack: Vec<Vec<Node>>,
    /// The dialect being lowered (gates zsh-only arms).
    lang: Language,
}

impl<'a> SharedBuilder<'a> {
    pub fn new(src: &'a [u8], lang: Language) -> Self {
        Self {
            src,
            stack: vec![Vec::new()], // module-level collector
            lang,
        }
    }

    /// The module-level node list (the single remaining collector).
    pub fn finish(mut self) -> Vec<Node> {
        self.stack.pop().expect("module collector")
    }

    /// Append a node to the current collector.
    pub fn emit(&mut self, node: Node) {
        self.stack.last_mut().expect("collector").push(node);
    }

    /// Run `f` against a fresh collector and return the nodes it gathered.
    pub fn collect<F: FnOnce(&mut Self)>(&mut self, f: F) -> Vec<Node> {
        self.stack.push(Vec::new());
        f(self);
        self.stack.pop().expect("collector")
    }

    /// The UTF-8 text of `node`, or `""` if it is not valid UTF-8.
    pub fn text(&self, node: TsNode) -> &str {
        node.utf8_text(self.src).unwrap_or("")
    }

    /// Recurse into every named child of `node` (skipping `extras`, i.e.
    /// comments). This is the "transparent" step shared by every arm that
    /// carries no score of its own: a fresh cursor walk with no intermediate
    /// `Vec` allocation.
    pub fn visit_named_children(&mut self, node: TsNode) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !child.is_extra() {
                self.visit(child);
            }
        }
    }

    // ---- traversal --------------------------------------------------------

    pub fn visit(&mut self, node: TsNode) {
        match node.kind() {
            "function_definition" => self.visit_function(node),

            "if_statement" => {
                let branch = self.lower_if(node);
                self.emit(branch);
            }

            "for_statement" | "c_style_for_statement" | "while_statement" => self.visit_loop(node),

            // zsh-only loop shapes: `repeat n do … done`, the terse
            // `for x (…) …`, and a standalone `select` (bash folds `select`
            // into `for_statement`, so its arm above already covers it).
            "repeat_statement" | "terse_for_statement" | "select_statement"
                if self.lang == Language::Zsh =>
            {
                self.visit_loop(node)
            }

            "case_statement" => self.visit_case(node),

            "ternary_expression" => self.visit_ternary(node),

            "list" | "binary_expression" => {
                if let Some(op) = self.logical_op_of(node) {
                    self.visit_logical(node, op);
                } else {
                    self.visit_named_children(node);
                }
            }

            "command" => self.visit_command(node),

            // zsh-only containers that carry no decision of their own: an
            // `always` block (a `finally`), a coprocess, and the brace group
            // the grammar splits out when an `always` follows it. They are
            // transparent; the explicit arms document that intent. (The
            // default arm would recurse identically.)
            "always_clause" | "compound_statement_no_always" | "coprocess_statement"
                if self.lang == Language::Zsh =>
            {
                self.visit_named_children(node)
            }

            // Everything else is transparent: recurse into every named child so
            // no nested construct is missed.
            _ => self.visit_named_children(node),
        }
    }

    /// A shell function is its own unit; the name is the `name` field (a
    /// `word`). The body (and any redirect) is walked in its own frame.
    fn visit_function(&mut self, node: TsNode) {
        let name = node
            .child_by_field_name("name")
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "<function>".into());
        let line = node.start_position().row as u32 + 1;
        let body = self.collect(|b| b.visit_named_children(node));
        self.emit(Node::Function {
            name,
            kind: "function".to_string(),
            line,
            body,
        });
    }

    /// Any loop shape (`for`, `select`, `while`, `until`, C-style `for (( ))`,
    /// zsh's `repeat` / terse `for`): the condition/initializer/update score
    /// inside the loop, matching the other adapters.
    fn visit_loop(&mut self, node: TsNode) {
        let body = self.collect(|b| b.visit_named_children(node));
        self.emit(Node::Loop { body });
    }

    /// A ternary inside `(( … ))`: a single increment.
    fn visit_ternary(&mut self, node: TsNode) {
        let test = node
            .child_by_field_name("condition")
            .map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let then = node
            .child_by_field_name("consequence")
            .map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        let alternate = node
            .child_by_field_name("alternative")
            .map_or_else(Vec::new, |c| self.collect(|b| b.visit(c)));
        self.emit(Node::Conditional {
            test,
            then,
            alternate,
        });
    }

    // ---- conditionals -----------------------------------------------------

    /// Build a `Branch` from an `if_statement`, folding its `elif`/`else` tail
    /// into `alternate`. The grammar does not field the condition/body split,
    /// so [`split_if_clause`](Self::split_if_clause) finds it at `then`.
    fn lower_if(&mut self, node: TsNode) -> Node {
        let (test, then) = self.split_if_clause(node);
        let alternatives = alternatives_of(node);
        let alternate = self.lower_alternatives(&alternatives);
        Node::Branch {
            test,
            then,
            alternate,
        }
    }

    /// Fold the `elif`/`else` run after an `if` (or `elif`): the first `elif`
    /// becomes a nested `Branch` that owns the rest of the run; an `else` is a
    /// flat `Group`.
    fn lower_alternatives(&mut self, alternatives: &[TsNode]) -> Option<Box<Node>> {
        let (first, rest) = alternatives.split_first()?;
        if first.kind() == "elif_clause" {
            let (test, then) = self.split_if_clause(*first);
            let alternate = self.lower_alternatives(rest);
            Some(Box::new(Node::Branch {
                test,
                then,
                alternate,
            }))
        } else {
            // An `else_clause` (defensively: anything else) ends the run flat.
            Some(Box::new(Node::Group(
                self.collect(|b| b.visit_named_children(*first)),
            )))
        }
    }

    /// Split one `if_statement`/`elif_clause` into its condition nodes (before
    /// the `then` keyword) and its body nodes (after `then`, before any
    /// `elif`/`else` clause). The grammar fields neither side, so the split is
    /// positional.
    fn split_if_clause(&mut self, node: TsNode) -> (Vec<Node>, Vec<Node>) {
        let mut test = Vec::new();
        let mut then = Vec::new();
        let mut saw_then = false;
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if !child.is_extra() {
                    if !child.is_named() {
                        if child.kind() == "then" {
                            saw_then = true;
                        }
                    } else if matches!(child.kind(), "elif_clause" | "else_clause") {
                        // Handled by the caller as alternatives; stop here.
                    } else if saw_then {
                        then.extend(self.collect(|b| b.visit(child)));
                    } else {
                        test.extend(self.collect(|b| b.visit(child)));
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        (test, then)
    }

    // ---- case -------------------------------------------------------------

    /// A `case` becomes a `Switch`: one `SwitchCase` per `case_item`, with a
    /// catch-all `*)` arm marked `is_default`. The subject expression(s) run at
    /// the switch's own level first.
    fn visit_case(&mut self, node: TsNode) {
        let mut cases = Vec::new();
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if !child.is_extra() {
                    match child.kind() {
                        // The `value` field on a direct child of `case` is the
                        // subject expression; walk it transparently.
                        "case_item" => {
                            let is_default = self.case_is_default(child);
                            let body = self.collect_case_body(child);
                            cases.push(SwitchCase { is_default, body });
                        }
                        _ if cursor.field_name() == Some("value") => self.visit(child),
                        _ => {}
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        self.emit(Node::Switch { cases });
    }

    /// A `case_item`'s body: every named child except the pattern `value`s.
    fn collect_case_body(&mut self, clause: TsNode) -> Vec<Node> {
        self.collect(|b| {
            let mut cursor = clause.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if !child.is_extra() && cursor.field_name() != Some("value") {
                        b.visit(child);
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        })
    }

    /// True for a catch-all `*)` arm: any pattern whose text is exactly `*`.
    fn case_is_default(&self, clause: TsNode) -> bool {
        let mut cursor = clause.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if cursor.field_name() == Some("value") && self.text(child).trim() == "*" {
                    return true;
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        false
    }

    // ---- logical operators ------------------------------------------------

    /// The normalized logical operator a `list` or `binary_expression`
    /// represents. `&&`/`||` are anonymous tokens; POSIX `-a`/`-o` sit in the
    /// `operator` field as a `test_operator`.
    fn logical_op_of(&self, node: TsNode) -> Option<LogicalOp> {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                match cursor.node().kind() {
                    "&&" => return Some(LogicalOp::And),
                    "||" => return Some(LogicalOp::Or),
                    _ => {}
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        if node.kind() == "binary_expression"
            && let Some(op) = node.child_by_field_name("operator")
            && op.kind() == "test_operator"
        {
            match self.text(op) {
                "-a" => return Some(LogicalOp::And),
                "-o" => return Some(LogicalOp::Or),
                _ => {}
            }
        }
        None
    }

    /// One folded [`Node::Logical`] for a run of like operators. A different
    /// operator nested inside starts a fresh `Logical`.
    fn visit_logical(&mut self, node: TsNode, op: LogicalOp) {
        let mut operands = Vec::new();
        for side in logical_operands(node) {
            self.collect_logical_side(side, op, &mut operands);
        }
        self.emit(Node::Logical { op, operands });
    }

    /// Flatten same-operator operands; a different operator nests as its own
    /// `Logical`; any other expression becomes a `Group` of its sub-nodes.
    fn collect_logical_side(&mut self, side: TsNode, op: LogicalOp, operands: &mut Vec<Node>) {
        match self.logical_op_of(side) {
            Some(side_op) => {
                let kids = logical_operands(side);
                if side_op == op {
                    for k in kids {
                        self.collect_logical_side(k, op, operands);
                    }
                } else {
                    let mut sub = Vec::new();
                    for k in kids {
                        self.collect_logical_side(k, side_op, &mut sub);
                    }
                    operands.push(Node::Logical {
                        op: side_op,
                        operands: sub,
                    });
                }
            }
            None => operands.push(Node::Group(self.collect(|b| b.visit(side)))),
        }
    }

    // ---- commands ---------------------------------------------------------

    /// Emit a `Call` (with the command's simple name for recursion detection)
    /// and recurse into the command's children (arguments may hold command
    /// substitutions, subshells, further calls, …). `break`/`continue` with a
    /// multi-level argument become a labelled `Jump` instead.
    fn visit_command(&mut self, node: TsNode) {
        let name = node.child_by_field_name("name").map(|n| self.text(n));
        match name {
            Some("break") | Some("continue") => {
                let labeled = self.has_multilevel_arg(node);
                self.emit(Node::Jump { labeled });
            }
            _ => {
                let callee = node
                    .child_by_field_name("name")
                    .and_then(|c| self.callee_name(c));
                self.emit(Node::Call { callee });
            }
        }
        self.visit_named_children(node);
    }

    /// True for `break 2` / `continue 2` (a jump past the innermost loop),
    /// which scores like a labelled jump. A non-literal argument is treated as
    /// a plain jump.
    fn has_multilevel_arg(&self, node: TsNode) -> bool {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "number" {
                return self.text(child).trim().parse::<u32>().is_ok_and(|n| n > 1);
            }
        }
        false
    }

    /// Simple name of a directly-invoked command (`echo`, `foo`, `./run.sh`),
    /// used for recursion detection. Names that are not a single literal word
    /// (e.g. `"$cmd"`) yield `None`.
    fn callee_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "word" => Some(self.text(node).to_string()),
            "command_name" => {
                let kids = named_children(node);
                match kids.as_slice() {
                    [only] if only.kind() == "word" => Some(self.text(*only).to_string()),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// The `elif_clause`/`else_clause` children of an `if_statement`, in order.
fn alternatives_of(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| matches!(c.kind(), "elif_clause" | "else_clause"))
        .collect()
}

/// The two operands of a logical node. A `binary_expression` keeps its operands
/// in `left`/`right` fields (its `operator` may be a named `test_operator`); a
/// `list` is a plain binary node whose operator is anonymous.
fn logical_operands(node: TsNode) -> Vec<TsNode> {
    if node.kind() == "binary_expression" {
        let mut v = Vec::new();
        if let Some(l) = node.child_by_field_name("left") {
            v.push(l);
        }
        if let Some(r) = node.child_by_field_name("right") {
            v.push(r);
        }
        v
    } else {
        named_children(node)
    }
}

/// The named children of `node` (skipping `extras`, i.e. comments), collected
/// into a `Vec` so the caller can index or slice-match them without holding the
/// cursor's borrow.
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| !c.is_extra())
        .collect()
}
