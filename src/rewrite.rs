//! Module for rewriting source text to reflect changes in the AST.
//!
//! We get two ASTs as input: an "old" AST, which was obtained by parsing some source text, and a
//! "new" AST, constructed by performing some arbitrary transformations on the old AST.  The goal
//! is to produce new source text corresponding to the new AST, while preserving as much as
//! possible the formatting, comments, and macro invocations as they appeared in the old source.
//!
//! Note that pretty-printing is a problem for macro invocations in particular - since the old AST
//! is macro-expanded before being transformed, each invocation would be replaced by its expansion.
//! Our goal is to preserve macro invocations as they appeared in the original source, but rewrite
//! inside macro arguments whenever possible.
//!
//! The strategy here is to traverse the new AST, with two modes of operation, depending on whether
//! we are in recycled code (nodes copied from the old AST) or fresh code (nodes copied from
//! replacements or generated from scratch).  The process also has a current output buffer, which
//! begins with the old source but is gradually transformed to reflect the structure of the new
//! AST.  If recycled code contains fresh code, then we need to delete the corresponding piece of
//! the output buffer and replace it with newly generated source for the fresh code.  Similarly, if
//! fresh code contains recycled code, we need to delete a piece and replace it with the
//! appropriate piece of the old source text.
//!
//! The process begins in "recycled" mode.  We traverse old and new ASTs together, checking for
//! differences between them.  If one is found, then the new AST has fresh code at this position.
//! We delete the buffer contents corresponding to the old AST node, and substitute in the result
//! of pretty-printing the new (fresh) node.  We then switch to "fresh" mode and recurse on the new
//! node, since it may contain additional chunks of recycled code as subtrees.
//!
//! "Fresh" mode is similar but uses a different condition to trigger rewriting.  When entering
//! "fresh" mode, we re-parse the pretty-printed text that was just substituted in, and traverse
//! the new AST and the reparsed AST.  These ASTs should be identical, but if a node in the new AST
//! has source information pointing to the old source text, then it is recycled code, and its text
//! in the output buffer needs to be replaced with the old source text.  We locate the piece of
//! text to be replaced by consulting the source information of the reparsed AST.  As before, when
//! rewriting occurs, we switch to "recycled" mode, in case the fresh node containis recycled code
//! as a subtree.  "Recycled" mode requires a copy of the old AST, which we obtain by looking up
//! the new node's source information in a precomputed table.


use std::collections::HashMap;
use std::mem;
use std::ops::{Deref, DerefMut};
use rustc::session::Session;
use syntax::ast::{Expr, ExprKind, Pat, Ty, Stmt, Item};
use syntax::ast::{NodeId, DUMMY_NODE_ID};
use syntax::codemap::{Span, DUMMY_SP};
use syntax::ptr::P;
use syntax::visit::{self, Visitor};

use visit::Visit;


pub trait Rewrite {
    /// Rewrite inside recycled code.  `self` is a node from the new AST; `old` is the
    /// corresponding node from the old AST.  Returns `true` if some rewriting of the nodes or
    /// their children is required, which happens if the nodes are unequal in visible ways.
    fn rewrite_recycled(&self, old: &Self, rcx: RewriteCtxtRef) -> bool;

    /// Rewrite inside fresh code.  `self` is a node from the new AST; `reparsed` is the
    /// corresponding node from the result of printing and then parsing the new AST.  Rewriting
    /// happens if the new node has a span referring to the old source code and there is an AST
    /// available for that span, but rewriting is handled immediately when needed and there is no
    /// need to propagate the information upward.
    fn rewrite_fresh(&self, reparsed: &Self, rcx: RewriteCtxtRef);
}


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextAdjust {
    None,
    Parenthesize,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextRewrite {
    pub old_span: Span,
    pub new_span: Span,
    pub rewrites: Vec<TextRewrite>,
    pub adjust: TextAdjust,
}


/// A table of nodes, each of which may or may not be "valid" according to some predicate.
pub struct NodeTable<'s, T: ?Sized+'s> {
    nodes: HashMap<NodeId, &'s T>,
}

impl<'s, T: ?Sized+::std::fmt::Debug> NodeTable<'s, T> {
    pub fn new() -> NodeTable<'s, T> {
        NodeTable {
            nodes: HashMap::new(),
        }
    }

    pub fn insert(&mut self, id: NodeId, node: &'s T) {
        if id == DUMMY_NODE_ID {
            return;
        }
        assert!(!self.nodes.contains_key(&id));
        self.nodes.insert(id, node);
    }

    pub fn get(&self, id: NodeId) -> Option<&'s T> {
        self.nodes.get(&id).map(|&x| x)
    }
}


struct OldNodes<'s> {
    exprs: NodeTable<'s, Expr>,
    pats: NodeTable<'s, Pat>,
    tys: NodeTable<'s, Ty>,
    stmts: NodeTable<'s, Stmt>,
    items: NodeTable<'s, Item>,
}

impl<'s> OldNodes<'s> {
    fn new() -> OldNodes<'s> {
        OldNodes {
            exprs: NodeTable::new(),
            pats: NodeTable::new(),
            tys: NodeTable::new(),
            stmts: NodeTable::new(),
            items: NodeTable::new(),
        }
    }
}


struct OldNodesVisitor<'s> {
    map: OldNodes<'s>,
}

impl<'s> Visitor<'s> for OldNodesVisitor<'s> {
    fn visit_expr(&mut self, x: &'s Expr) {
        if let ExprKind::Paren(_) = x.node {
            // Ignore.  `Paren` nodes cause problems because they have the same NodeId as the inner
            // expression.
        } else {
            self.map.exprs.insert(x.id, x);
        }
        visit::walk_expr(self, x);
    }

    fn visit_pat(&mut self, x: &'s Pat) {
        self.map.pats.insert(x.id, x);
        visit::walk_pat(self, x);
    }

    fn visit_ty(&mut self, x: &'s Ty) {
        self.map.tys.insert(x.id, x);
        visit::walk_ty(self, x);
    }

    fn visit_stmt(&mut self, x: &'s Stmt) {
        self.map.stmts.insert(x.id, x);
        visit::walk_stmt(self, x);
    }

    fn visit_item(&mut self, x: &'s Item) {
        self.map.items.insert(x.id, x);
        visit::walk_item(self, x);
    }
}


/// A record of a single step in the AST traversal.  We care mainly about the nesting of
/// `ExprKind`s, since it affects parenthesization of expressions.
#[derive(Clone, Debug)]
pub enum VisitStep {
    /// Stepped from an `ExprKind` into one of its children.
    Expr(P<ExprKind>),
    /// Stepped from an `ExprKind` into its left child.
    ExprLeft(P<ExprKind>),
    /// Stepped from an `ExprKind` into its right child.
    ExprRight(P<ExprKind>),
    /// Stepped from some other node into one of its children.
    Other,
}

impl VisitStep {
    pub fn get_expr_kind(&self) -> Option<&ExprKind> {
        match *self {
            VisitStep::Expr(ref k) => Some(k),
            VisitStep::ExprLeft(ref k) => Some(k),
            VisitStep::ExprRight(ref k) => Some(k),
            _ => None,
        }
    }

    pub fn is_left(&self) -> bool {
        match *self {
            VisitStep::ExprLeft(_) => true,
            _ => false,
        }
    }

    pub fn is_right(&self) -> bool {
        match *self {
            VisitStep::ExprRight(_) => true,
            _ => false,
        }
    }
}


pub struct RewriteCtxt<'s> {
    sess: &'s Session,
    old_nodes: OldNodes<'s>,

    /// The span of the new AST the last time we entered "fresh" mode.  This lets us avoid infinite
    /// recursion - see comment in `splice_fresh`.
    fresh_start: Span,

    visit_steps: Vec<VisitStep>,
}

impl<'s> RewriteCtxt<'s> {
    fn new(sess: &'s Session, old_nodes: OldNodes<'s>) -> RewriteCtxt<'s> {
        RewriteCtxt {
            sess: sess,
            old_nodes: old_nodes,

            fresh_start: DUMMY_SP,
            visit_steps: Vec::new(),
        }
    }

    pub fn session(&self) -> &'s Session {
        self.sess
    }

    pub fn old_exprs(&mut self) -> &mut NodeTable<'s, Expr> {
        &mut self.old_nodes.exprs
    }

    pub fn old_pats(&mut self) -> &mut NodeTable<'s, Pat> {
        &mut self.old_nodes.pats
    }

    pub fn old_tys(&mut self) -> &mut NodeTable<'s, Ty> {
        &mut self.old_nodes.tys
    }

    pub fn old_stmts(&mut self) -> &mut NodeTable<'s, Stmt> {
        &mut self.old_nodes.stmts
    }

    pub fn old_items(&mut self) -> &mut NodeTable<'s, Item> {
        &mut self.old_nodes.items
    }

    pub fn fresh_start(&self) -> Span {
        self.fresh_start
    }

    pub fn replace_fresh_start(&mut self, span: Span) -> Span {
        mem::replace(&mut self.fresh_start, span)
    }

    pub fn with_rewrites<'b>(&'b mut self,
                             rewrites: &'b mut Vec<TextRewrite>)
                             -> RewriteCtxtRef<'s, 'b> {
        RewriteCtxtRef {
            rewrites: rewrites,
            cx: self,
        }
    }

    pub fn push_step(&mut self, step: VisitStep) {
        self.visit_steps.push(step);
    }

    pub fn pop_step(&mut self) {
        self.visit_steps.pop();
    }

    pub fn parent_step(&self) -> Option<&VisitStep> {
        self.visit_steps.last()
    }
}


pub struct RewriteCtxtRef<'s: 'a, 'a> {
    rewrites: &'a mut Vec<TextRewrite>,
    cx: &'a mut RewriteCtxt<'s>,
}

impl<'s, 'a> Deref for RewriteCtxtRef<'s, 'a> {
    type Target = RewriteCtxt<'s>;

    fn deref(&self) -> &RewriteCtxt<'s> {
        self.cx
    }
}

impl<'s, 'a> DerefMut for RewriteCtxtRef<'s, 'a> {
    fn deref_mut(&mut self) -> &mut RewriteCtxt<'s> {
        self.cx
    }
}

impl<'s, 'a> RewriteCtxtRef<'s, 'a> {
    pub fn borrow<'b>(&'b mut self) -> RewriteCtxtRef<'s, 'b> {
        RewriteCtxtRef {
            rewrites: self.rewrites,
            cx: self.cx,
        }
    }

    pub fn with_rewrites<'b>(&'b mut self,
                             rewrites: &'b mut Vec<TextRewrite>)
                             -> RewriteCtxtRef<'s, 'b> {
        RewriteCtxtRef {
            rewrites: rewrites,
            cx: self.cx,
        }
    }

    pub fn mark(&self) -> usize {
        self.rewrites.len()
    }

    pub fn rewind(&mut self, mark: usize) {
        self.rewrites.truncate(mark);
    }

    pub fn record(&mut self,
                  old_span: Span,
                  new_span: Span,
                  rewrites: Vec<TextRewrite>,
                  adjust: TextAdjust) {
        self.rewrites.push(TextRewrite {
            old_span: old_span,
            new_span: new_span,
            rewrites: rewrites,
            adjust: adjust,
        });
    }
}


pub fn rewrite<T: Rewrite+Visit>(sess: &Session, old: &T, new: &T) -> Vec<TextRewrite> {
    let mut v = OldNodesVisitor { map: OldNodes::new() };
    old.visit(&mut v);

    let mut rcx = RewriteCtxt::new(sess, v.map);
    let mut rewrites = Vec::new();
    let need_rewrite = Rewrite::rewrite_recycled(new, old, rcx.with_rewrites(&mut rewrites));
    assert!(!need_rewrite, "rewriting did not complete");
    rewrites
}
